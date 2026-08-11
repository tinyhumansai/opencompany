//! One agent, opened: the detail read and the edit behind `GET`/`PATCH
//! {scope}/team/{agent_id}` (issue #264).
//!
//! Before this, an agent was a dead end. `GET …/team` returned a name, a role
//! and a description; nothing carried the agent's tier, its tool grants or its
//! desks, and there was no per-agent route at all. So the console could show a
//! card and offer to delete it, and that was the whole of what an operator
//! could learn or do. Worse than the missing screen: **checking what tools a
//! company actually grants an agent had no read surface anywhere**, which is
//! why a tool-grant change could not be verified from outside the process.
//!
//! ## Effective, not declared
//!
//! [`AgentToolsDto`] carries three lists rather than one, because the
//! interesting number is the one nobody could see. `requested` is what the
//! `[[agent]].tools` line asks for, `companyAllow` is the `[tools].allow`
//! ceiling it is intersected with, and `effective` is what the agent actually
//! ends up holding. An agent that requests `workspace.read` under a company
//! that allows only `composio` requests one tool and holds none, and a surface
//! that printed the request alone would report the opposite of the truth.
//!
//! `effective` is computed by
//! [`agent_effective_grants`](crate::runtime::builder::agent_effective_grants)
//! — the *same* function the harness calls when it builds the agent, not a
//! re-implementation of the rule. A second copy would eventually disagree, and
//! a tool-grant readout that disagrees with the harness is worse than none.
//!
//! ## What may be edited, and why that is the line
//!
//! The console edits what the console owns.
//!
//! An **overlay** teammate — one an operator defined through "Define an agent",
//! or the orchestrator created with `add_agent` — lives on the
//! [`CompanyRecord`], which this process writes. Its name, role, description
//! and (since issue #619) its tool scope are editable here, and that is the
//! whole of #264's "the roster is write-once per member" complaint: before
//! this, iterating on an agent's instructions meant deleting it and starting
//! over.
//!
//! A **manifest** teammate is declared in the version-controlled `company.toml`
//! and is not editable from a browser. This is the same line
//! [`MANIFEST_TEAMMATE_DELETE`](super::language::MANIFEST_TEAMMATE_DELETE)
//! already draws for removal, and it is drawn in the same place for the same
//! reason: the manifest is the company's blueprint, the overlay model exists so
//! the runtime never rewrites it, and a console PATCH that edited it would make
//! the deployed company silently diverge from the file in git.
//!
//! The one field that *is* editable on a manifest teammate — its daily budget —
//! is editable precisely because #343 modelled it as a
//! [`BudgetOverride`](crate::ports::types::BudgetOverride) layered on top rather
//! than as a rewrite. It keeps its own route and is untouched here.
//!
//! `tier` is read-only for **both** kinds. There is no override layer for it,
//! and inventing one is a policy decision, not something to smuggle into a
//! detail view.
//!
//! `tools` was read-only for the same reason until issue #619 made that policy
//! decision for the overlay half — deliberately, and only for the half this
//! process owns. The privilege question is real and the answer is that a
//! manifest teammate's tool line still lives in git and is still a `409`; what
//! changed is that an *overlay* teammate had no tool line at all, so it held
//! the company's widest grant permanently with no way to say otherwise. The
//! company `[tools].allow` ceiling is unchanged by this route, so the edit can
//! only ever narrow a teammate within a grant the company already made.
//!
//! The server states the rule rather than leaving the console to re-derive it:
//! every detail response carries an [`editable`](AgentDetailDto::editable) list,
//! and the console renders a field read-only exactly when the host says it is.
//! A console that decided this for itself would drift from what the host
//! actually accepts, and the operator would meet the disagreement as a failed
//! save.

use axum::Json;
use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use axum::routing::{self, MethodRouter};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::store::company_write_lock;
use crate::ports::types::CompanyRecord;
use crate::runtime::builder::agent_effective_grants;
use crate::server::error::ApiError;
use crate::server::ops::ScopedCompany;
use crate::server::ops::language;
use crate::server::ops::team::{AgentPath, daily_spend_samples, double_option};

/// The `{scope}/team/{agent_id}` fragment: read one agent, edit one agent.
///
/// Merged into [`super::team::router`]'s existing `/team/{agent_id}` entry
/// rather than declared as its own route — axum panics on two routers claiming
/// one path, even for disjoint methods.
pub(super) fn method_router() -> MethodRouter<AppState> {
    routing::get(agent_detail).patch(edit_agent)
}

/// Which half of the roster a teammate comes from, and therefore what may be
/// done to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum AgentSource {
    /// Declared in the version-controlled `company.toml`.
    Manifest,
    /// Added at runtime by an operator or the orchestrator, stored on the
    /// company record.
    Overlay,
}

/// The fields a `PATCH` accepts for an overlay teammate. Sent to the console so
/// it renders the same rule the host enforces.
const OVERLAY_EDITABLE: [&str; 4] = ["name", "role", "description", "tools"];

/// One agent, in full — everything #264 lists as unreachable.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentDetailDto {
    id: String,
    /// Absent for a manifest teammate, which is named by its role. Same rule as
    /// `GET …/team`.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    role: String,
    /// What the agent was defined with. This is the text that frames the
    /// agent's persona for every turn it takes, which is what the issue means
    /// by "the `AGENT.md` or similar file for that agent" — the manifest
    /// already carries it, the console just never showed it after creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    source: AgentSource,
    /// The field names a `PATCH` will accept for this teammate. Empty for a
    /// manifest teammate.
    editable: Vec<&'static str>,
    /// The declared cognition-tier hint, when the manifest sets one. An overlay
    /// teammate has none by construction.
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
    /// Whether this teammate is the company's orchestrator — resolved by the
    /// roster rule (tagged tier first, else the first declared agent), not read
    /// off `tier` alone, so an untagged roster's real orchestrator is named.
    is_orchestrator: bool,
    tools: AgentToolsDto,
    desks: Vec<AgentDeskDto>,
    inbox_enabled: bool,
    /// The cap in force, its spend, and its attribution — the same fields and
    /// the same absent-means-uncapped contract as `GET …/team`.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_usd_daily: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spent_today_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_set_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_set_at_millis: Option<u64>,
}

/// An agent's tool grants at all three levels, so the resolution is legible
/// rather than asserted.
///
/// Built **only** through [`agent_tools`], so every surface that renders an
/// agent's tools renders the same list — see that function for why that is a
/// rule rather than a convenience.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentToolsDto {
    /// The globs the agent asks for. **Empty means "the company's standard
    /// grant"**, not "no tools" — an agent that lists nothing inherits the whole
    /// allow-list. The console has to say which of the two it is looking at.
    requested: Vec<String>,
    /// The company-wide `[tools].allow` ceiling.
    company_allow: Vec<String>,
    /// What the agent actually holds, after the intersection.
    effective: Vec<String>,
}

/// A desk this agent sits on.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AgentDeskDto {
    id: String,
    name: String,
    /// Whether this agent is the desk's lead — the first effective member, who
    /// receives a `delegate_to_desk` hand-off.
    lead: bool,
}

/// The tool globs an agent *asks* for, resolved identically for every reader.
///
/// A manifest teammate's `[[agent]].tools` line, or an overlay teammate's
/// [`OverlayAgent::tools`](crate::ports::types::OverlayAgent::tools) — which
/// mirrors `harness::overlay_agent_to_manifest`, deliberately and in lockstep
/// (issue #619). Either one being **empty** means "the company's standard
/// grant", not "no tools".
///
/// Before #619 the overlay half was hard-coded empty here, because an overlay
/// teammate had no tools field to read. Now that it has one, reading it is what
/// keeps this route from showing an operator a grant the harness does not
/// honour — the verification gap #264 exists to close.
///
/// Its callers have already established that `agent_id` is on the roster, so a
/// miss on both halves can only be a caller that skipped that check.
pub(super) fn requested_grants(record: &CompanyRecord, agent_id: &str) -> Vec<String> {
    record
        .manifest
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .map(|agent| agent.tools.clone())
        .or_else(|| {
            record
                .overlay_agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .map(|agent| agent.tools.clone())
        })
        .unwrap_or_default()
}

/// The **declared** cognition-tier hint for `agent_id`: the manifest
/// `[[agent]].tier` line verbatim, or `None` when the row declares none — and
/// for every overlay teammate, which has no manifest row to declare one.
///
/// Verbatim is the whole contract. This is what the company *wrote*, not a
/// resolved answer, and `None` means **undeclared** — a reader has to render
/// that as "cannot say" rather than substituting a default. Issue #643 is
/// exactly that substitution: the overview graph printed a literal `worker` for
/// every teammate, so a company declaring `tier = "orchestrator"` read back as
/// a worker on its own graph.
///
/// Sibling of [`requested_grants`] in shape and in reason: one lookup, shared
/// by the roster list and the detail read, so the two cannot answer differently
/// for the same teammate.
pub(super) fn declared_tier(record: &CompanyRecord, agent_id: &str) -> Option<String> {
    record
        .manifest
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.tier.clone())
}

/// Whether `agent_id` is this company's orchestrator.
///
/// Delegates to [`crate::company::orchestrator_id`] — the roster rule the
/// harness itself resolves the orchestrator with (the agent tagged with the
/// orchestrator tier, else the first declared agent), never a re-read of
/// [`declared_tier`].
///
/// **This is not the same question as the tier.** A company that tags nobody
/// still has an orchestrator, so an untagged first agent answers `true` here
/// while [`declared_tier`] answers `None`; and a *second* agent tagged with the
/// orchestrator tier carries that tier while answering `false` here, because the
/// rule picks one. A caller that re-derived the marker from the tier string
/// would get both of those backwards.
pub(super) fn is_orchestrator(record: &CompanyRecord, agent_id: &str) -> bool {
    crate::company::orchestrator_id(&record.manifest.agents) == Some(agent_id)
}

/// One agent's grants at all three levels — the single constructor for
/// [`AgentToolsDto`].
///
/// `effective` comes from
/// [`agent_effective_grants`](crate::runtime::builder::agent_effective_grants),
/// the same function the harness builds the agent with, for the reason the
/// module docs give. This function exists so the **roster list** and the
/// **detail read** cannot answer that question differently either (issue
/// #601): the overview graph reads the list and used to invent a tool shelf by
/// dealing slices of `[tools].allow`, so the graph and the detail card beside
/// it disagreed about the same agent. Sharing the constructor makes that
/// disagreement unrepresentable rather than merely fixed once.
pub(super) fn agent_tools(company_allow: &[String], requested: Vec<String>) -> AgentToolsDto {
    let effective = agent_effective_grants(company_allow, &requested);
    AgentToolsDto {
        requested,
        company_allow: company_allow.to_vec(),
        effective,
    }
}

/// The `PATCH` body. Every field is optional, and an absent field is left
/// alone: this is a patch, not a replacement, so a console that renders only
/// some of an agent's fields cannot blank the rest by omission.
///
/// `description` is a **double option** so "leave it" and "clear it" stay
/// apart on the wire, the same shape and for the same reason as
/// [`SetBudget`](super::team::SetBudget)'s cap:
///
/// | body | parses as | means |
/// |---|---|---|
/// | `{}` | `None` | leave the description alone |
/// | `{"description": null}` | `Some(None)` | clear it |
/// | `{"description": "…"}` | `Some(Some(…))` | set it |
///
/// Collapsing the first two would make every partial save silently erase an
/// agent's instructions, which is the single worst thing this route could do.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditAgent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    description: Option<Option<String>>,
    /// The teammate's tool scope (issue #619). Absent leaves it alone; an
    /// **empty array** is the deliberate way back to the company's standard
    /// grant, which is why this is a plain `Option` and not a double option —
    /// `[]` already spells "clear it" without needing `null` to mean something
    /// different from omission.
    #[serde(default)]
    tools: Option<Vec<String>>,
}

/// `GET {scope}/team/{agent_id}` — one agent, read.
async fn agent_detail(
    company: ScopedCompany,
    Path(AgentPath { agent_id }): Path<AgentPath>,
) -> Result<Json<AgentDetailDto>, ApiError> {
    let record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;
    detail(&company, &record, &agent_id).await
}

/// `PATCH {scope}/team/{agent_id}` — edit an overlay teammate.
///
/// Refuses a manifest teammate with a `409` naming where the edit belongs, and
/// an unknown id with a `404`. Open to any signed-in member, matching `POST
/// …/team`: defining a teammate was never admin-only, so correcting one it
/// defined is not either. Setting a *budget* still is, on its own route.
async fn edit_agent(
    company: ScopedCompany,
    Path(AgentPath { agent_id }): Path<AgentPath>,
    Json(body): Json<EditAgent>,
) -> Result<Json<AgentDetailDto>, Response> {
    // Serialize with every other write to `overlay_agents`, so a console edit
    // and a concurrent `add_agent` cannot clobber one another's roster.
    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = company
        .runtime
        .store()
        .load(company.id())
        .await
        .map_err(|e| ApiError(e).into_response())?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(company.id().to_string())).into_response()
        })?;

    // Identity before validation, so an unknown id is a 404 rather than a
    // complaint about the shape of a body nobody could have applied anyway.
    if record.manifest.agents.iter().any(|a| a.id == agent_id) {
        return Err(ApiError(OpenCompanyError::Conflict(
            language::MANIFEST_TEAMMATE_EDIT.to_string(),
        ))
        .into_response());
    }
    if !record.overlay_agents.iter().any(|a| a.id == agent_id) {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
            "teammate {agent_id}"
        )))
        .into_response());
    }

    let name = trimmed_field(body.name.as_deref(), "name").map_err(|e| e.into_response())?;
    let role = trimmed_field(body.role.as_deref(), "role").map_err(|e| e.into_response())?;
    let tools = body
        .tools
        .map(|globs| trimmed_globs(&globs))
        .transpose()
        .map_err(|e| e.into_response())?;

    {
        let agent = record
            .overlay_agents
            .iter_mut()
            .find(|a| a.id == agent_id)
            .expect("overlay membership was checked above");
        if let Some(name) = name {
            agent.name = name;
        }
        if let Some(role) = role {
            agent.role = role;
        }
        // Present-and-null clears; a blank string clears too, since an empty
        // description and no description frame the persona identically and
        // storing `Some("")` would only make the two look different on the wire.
        if let Some(description) = body.description {
            agent.description = description
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
        }
        // Issue #619: the one place an operator can narrow an overlay teammate.
        // Stored verbatim, exactly like a manifest `[[agent]].tools` line — the
        // company `allow` ceiling is applied at *read* time by
        // `agent_effective_grants`, so a glob the company does not cover is
        // surfaced as asked-for-but-not-granted rather than silently dropped
        // here.
        if let Some(tools) = tools {
            agent.tools = tools;
        }
    }

    company
        .runtime
        .store()
        .save(&record)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    detail(&company, &record, &agent_id)
        .await
        .map_err(|e| e.into_response())
}

/// Rejects a field that was sent but is blank, and trims one that was sent.
///
/// A teammate whose name is whitespace renders as an empty card with no way
/// back to it, so the refusal is a `400` rather than a stored blank.
///
/// The error is an [`ApiError`], **not** the `Response` its caller returns.
/// `clippy::result_large_err` fires on the second shape here and is right to:
/// an `axum` `Response` is 128+ bytes, so a `Result<Option<String>, Response>`
/// makes every successful call carry the footprint of the refusal it did not
/// make. The handler is exempt only because its own `Ok` variant is larger
/// still. The caller converts at the boundary, which is also what the sibling
/// refusal helpers in `team.rs` do by returning `Option<Response>`.
fn trimmed_field(value: Option<&str>, field: &str) -> Result<Option<String>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "a teammate's {field} can't be empty."
        ))));
    }
    Ok(Some(trimmed.to_string()))
}

/// Trims a submitted tool-scope list, refusing a blank entry and dropping
/// duplicates (issue #619).
///
/// A blank glob is a `400` rather than a stored empty string for a sharper
/// reason than tidiness: `""` matches nothing an operator meant, so it would
/// read as a scope that grants nothing while looking like a scope that was set.
/// Duplicates are dropped rather than refused — a repeated glob is harmless and
/// the resolved grant list is de-duplicated downstream anyway.
///
/// Same `ApiError`-not-`Response` return shape as [`trimmed_field`], for the
/// reason given there.
fn trimmed_globs(globs: &[String]) -> Result<Vec<String>, ApiError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(globs.len());
    for glob in globs {
        let trimmed = glob.trim();
        if trimmed.is_empty() {
            return Err(ApiError(OpenCompanyError::InvalidRequest(
                "a tool grant can't be empty. Send an empty list to give this teammate the company's standard grant.".to_string(),
            )));
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

/// Builds one agent's detail from the loaded record, or 404s when the id names
/// nobody on the roster.
async fn detail(
    company: &ScopedCompany,
    record: &CompanyRecord,
    agent_id: &str,
) -> Result<Json<AgentDetailDto>, ApiError> {
    let manifest_agent = record.manifest.agents.iter().find(|a| a.id == agent_id);
    let overlay_agent = record.overlay_agents.iter().find(|a| a.id == agent_id);

    let (source, name, role, description) = match (manifest_agent, overlay_agent) {
        // A manifest agent wins an id collision, exactly as `build_roster`
        // resolves one: the version-controlled roster is authoritative.
        (Some(agent), _) => (
            AgentSource::Manifest,
            None,
            agent.role.clone(),
            agent.description.clone(),
        ),
        // An overlay teammate has no manifest row, so `declared_tier` below
        // misses — and so does `requested_grants`: it holds the company's
        // standard grant, mirroring `harness::overlay_agent_to_manifest`.
        (None, Some(agent)) => (
            AgentSource::Overlay,
            Some(agent.name.clone()),
            agent.role.clone(),
            agent.description.clone(),
        ),
        (None, None) => {
            return Err(ApiError(OpenCompanyError::CompanyNotFound(format!(
                "teammate {agent_id}"
            ))));
        }
    };

    let cap = record.effective_budget(agent_id);
    let attribution = record.budget_override(agent_id);
    let spend_today = daily_spend_samples(company, Some(record)).await?;
    let spent = cap.and(
        spend_today
            .as_ref()
            .map(|samples| crate::metering::usd_spent_by_agent(samples, agent_id)),
    );

    let inbox_enabled = company
        .runtime
        .inbox()
        .inboxes(company.id())
        .await?
        .into_iter()
        .any(|meta| meta.key == agent_id && meta.enabled);

    Ok(Json(AgentDetailDto {
        id: agent_id.to_string(),
        name,
        role,
        description,
        source,
        editable: match source {
            AgentSource::Overlay => OVERLAY_EDITABLE.to_vec(),
            AgentSource::Manifest => Vec::new(),
        },
        tier: declared_tier(record, agent_id),
        is_orchestrator: is_orchestrator(record, agent_id),
        tools: agent_tools(
            &record.manifest.tools.allow,
            requested_grants(record, agent_id),
        ),
        desks: desks_for(record, agent_id),
        inbox_enabled,
        budget_usd_daily: cap,
        spent_today_usd: spent,
        budget_set_by: attribution.map(|entry| entry.set_by.id.clone()),
        budget_set_at_millis: attribution.map(|entry| entry.at_millis),
    }))
}

/// Every desk this agent is an effective member of, manifest desks first.
///
/// Resolved through
/// [`CompanyRecord::effective_desk_members`](crate::ports::types::CompanyRecord::effective_desk_members)
/// rather than by reading the declared member lists, so an operator-added
/// membership and an operator-set lead order are both reflected — the same
/// answer the Desks page and the harness `desk_lead` resolver give.
///
/// Shared with `GET {scope}/team` (issue #601) for the same anti-drift reason
/// as [`agent_tools`]: desks are the overview graph's departments now, so the
/// roster list and this read have to agree on which desks a teammate sits on.
pub(super) fn desks_for(record: &CompanyRecord, agent_id: &str) -> Vec<AgentDeskDto> {
    let declared = record
        .manifest
        .group_chats
        .iter()
        .map(|chat| (chat.id.as_str(), chat.name.as_str()))
        .chain(
            record
                .overlay_desks
                .iter()
                .map(|desk| (desk.id.as_str(), desk.name.as_str())),
        );
    declared
        .filter_map(|(id, name)| {
            let members = record.effective_desk_members(id);
            members.iter().any(|m| m == agent_id).then(|| AgentDeskDto {
                id: id.to_string(),
                name: name.to_string(),
                lead: members.first().map(String::as_str) == Some(agent_id),
            })
        })
        .collect()
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
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    /// A company whose grants actually bite: `ceo` asks for one tool the company
    /// does not allow, `writer` asks for nothing at all, and `hermit` sits on no
    /// desk. Each of those is a different arm of the resolution under test.
    const ROSTER: &str = r#"
[company]
name = "Acme"
[policy]
mode = "full"
[tools]
allow = ["workspace", "workspace.*", "composio"]

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction and delegates."
tier = "orchestrator"
tools = ["workspace.read", "email.send"]

[[agent]]
id = "writer"
role = "Writer"

[[agent]]
id = "hermit"
role = "Hermit"

[[group_chat]]
id = "content"
name = "Content desk"
members = ["writer", "ceo"]
"#;

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-agent-detail-")
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
                overlay_policy: None,
                disabled_workflows: Vec::new(),
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

    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", crate::server::test_support::fixed_cookie("acme"));
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

    async fn get_agent(state: &AppState, agent: &str) -> (StatusCode, Value) {
        send(state, "GET", &format!("/api/v1/company/team/{agent}"), None).await
    }

    async fn patch_agent(state: &AppState, agent: &str, body: Value) -> (StatusCode, Value) {
        send(
            state,
            "PATCH",
            &format!("/api/v1/company/team/{agent}"),
            Some(body),
        )
        .await
    }

    /// Adds a teammate through the console's own route and returns its id.
    async fn add_overlay(state: &AppState, name: &str, role: &str) -> String {
        let (status, created) = send(
            state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": name, "role": role, "description": "Original."})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        created["id"].as_str().unwrap().to_string()
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .unwrap_or_else(|| panic!("expected an array, got {value}"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    // --- The read half (issue #264) -----------------------------------------

    /// The whole of what the issue calls unreachable, on the wire: tier,
    /// description, resolved tools and desk membership for a manifest teammate.
    #[tokio::test]
    async fn a_manifest_agent_opens_with_its_tier_tools_and_desks() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, ceo) = get_agent(&state, "ceo").await;
        assert_eq!(status, StatusCode::OK, "{ceo}");

        assert_eq!(ceo["id"], "ceo");
        assert_eq!(ceo["role"], "Chief Executive");
        assert_eq!(ceo["description"], "Sets direction and delegates.");
        assert_eq!(ceo["source"], "manifest");
        assert_eq!(ceo["tier"], "orchestrator");
        assert_eq!(ceo["isOrchestrator"], true, "{ceo}");
        assert!(
            ceo["name"].is_null(),
            "a manifest teammate is named by its role: {ceo}"
        );

        // Desk membership, with the lead flag resolved from the effective order
        // rather than from the declared list — `writer` is declared first.
        let desks = ceo["desks"].as_array().unwrap();
        assert_eq!(desks.len(), 1, "{ceo}");
        assert_eq!(desks[0]["id"], "content");
        assert_eq!(desks[0]["name"], "Content desk");
        assert_eq!(desks[0]["lead"], false, "the writer leads this desk: {ceo}");

        // A teammate on no desk says so with an empty list rather than by
        // omitting the key, so the console can render "no desks" for sure.
        let (_, hermit) = get_agent(&state, "hermit").await;
        assert_eq!(hermit["desks"].as_array().unwrap().len(), 0, "{hermit}");
        assert_eq!(hermit["isOrchestrator"], false, "{hermit}");
    }

    /// The verification gap the issue names: what an agent *asks* for and what
    /// it *holds* are different lists, and only the second one matters.
    ///
    /// `ceo` requests `email.send`, which `[tools].allow` does not cover, so it
    /// is dropped. `writer` requests nothing, which means the company's standard
    /// grant rather than no tools at all — the opposite reading, and the one a
    /// naive surface would get wrong.
    #[tokio::test]
    async fn effective_tools_are_the_intersection_not_the_request() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (_, ceo) = get_agent(&state, "ceo").await;
        assert_eq!(
            strings(&ceo["tools"]["requested"]),
            vec!["workspace.read", "email.send"],
            "{ceo}"
        );
        assert_eq!(
            strings(&ceo["tools"]["companyAllow"]),
            vec!["workspace", "workspace.*", "composio"],
            "{ceo}"
        );
        assert_eq!(
            strings(&ceo["tools"]["effective"]),
            vec!["workspace.read"],
            "a request the company never allowed is not a grant: {ceo}"
        );

        let (_, writer) = get_agent(&state, "writer").await;
        assert!(
            strings(&writer["tools"]["requested"]).is_empty(),
            "{writer}"
        );
        assert_eq!(
            strings(&writer["tools"]["effective"]),
            vec!["workspace", "workspace.*", "composio"],
            "an agent that lists no tools holds the company's whole allow-list, \
             which is the reading a surface must not invert: {writer}"
        );
    }

    /// A roster that tags nobody still has an orchestrator: the first declared
    /// agent. A console that read `tier` alone would call every teammate on such
    /// a company a worker, and be wrong about all of them.
    #[tokio::test]
    async fn an_untagged_roster_still_names_an_orchestrator() {
        let home_dir = home();
        let state = state_with_manifest(
            home_dir.path(),
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
             [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n\
             [[agent]]\nid = \"editor\"\nrole = \"Editor\"\n",
        )
        .await;

        let (_, writer) = get_agent(&state, "writer").await;
        assert!(writer["tier"].is_null(), "{writer}");
        assert_eq!(writer["isOrchestrator"], true, "{writer}");

        let (_, editor) = get_agent(&state, "editor").await;
        assert_eq!(editor["isOrchestrator"], false, "{editor}");
    }

    /// An operator-added membership counts: the detail view resolves desks
    /// through `effective_desk_members`, not through the manifest's list.
    #[tokio::test]
    async fn an_operator_added_desk_membership_shows_on_the_agent() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        assert_eq!(
            get_agent(&state, "hermit").await.1["desks"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        let (status, _) = send(
            &state,
            "POST",
            "/api/v1/company/desks/content/members",
            Some(json!({"agent_id": "hermit"})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (_, hermit) = get_agent(&state, "hermit").await;
        let desks = hermit["desks"].as_array().unwrap();
        assert_eq!(desks.len(), 1, "{hermit}");
        assert_eq!(desks[0]["id"], "content", "{hermit}");
    }

    /// An overlay teammate reads back with the company's standard grant and no
    /// tier, which is exactly what the harness builds it with.
    #[tokio::test]
    async fn an_overlay_teammate_reports_the_standard_grant() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let jamie = add_overlay(&state, "Jamie", "Growth").await;

        let (status, agent) = get_agent(&state, &jamie).await;
        assert_eq!(status, StatusCode::OK, "{agent}");
        assert_eq!(agent["source"], "overlay");
        assert_eq!(agent["name"], "Jamie");
        assert!(agent["tier"].is_null(), "{agent}");
        assert_eq!(agent["isOrchestrator"], false, "{agent}");
        assert!(strings(&agent["tools"]["requested"]).is_empty(), "{agent}");
        assert_eq!(
            strings(&agent["tools"]["effective"]),
            vec!["workspace", "workspace.*", "composio"],
            "{agent}"
        );
    }

    /// Issue #601: the roster **list** answers for tools and desks too, with
    /// the same values as the detail read.
    ///
    /// The overview graph is drawn from the list, so before this it had no way
    /// to learn either without an N+1 fetch — and invented both instead, while
    /// the detail card beside it rendered the real thing. The equality is the
    /// contract; anything less lets the two surfaces disagree again.
    #[tokio::test]
    async fn the_roster_list_carries_the_same_tools_and_desks_as_the_detail_read() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        // An overlay teammate too, so the agreement is checked on both halves
        // of the merged roster rather than only on the manifest half.
        let jamie = add_overlay(&state, "Jamie", "Growth").await;

        let (status, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
        assert_eq!(status, StatusCode::OK, "{roster}");
        let rows = roster.as_array().unwrap();
        assert_eq!(rows.len(), 4, "{roster}");

        for row in rows {
            let id = row["id"].as_str().unwrap();
            let (_, detail) = get_agent(&state, id).await;
            assert_eq!(
                row["tools"], detail["tools"],
                "the graph reads the list and the card reads the detail; they \
                 must not disagree about {id}"
            );
            assert_eq!(row["desks"], detail["desks"], "desks disagree for {id}");
        }

        let row_of = |id: &str| {
            rows.iter()
                .find(|row| row["id"] == id)
                .unwrap_or_else(|| panic!("{id} missing from {roster}"))
                .clone()
        };

        // …and the shared values are the *right* ones, so a shared-but-wrong
        // constructor cannot pass on agreement alone.
        let ceo = row_of("ceo");
        assert_eq!(
            strings(&ceo["tools"]["effective"]),
            vec!["workspace.read"],
            "a request the company never allowed is not a grant: {ceo}"
        );
        let writer = row_of("writer");
        assert!(
            strings(&writer["tools"]["requested"]).is_empty(),
            "{writer}"
        );
        assert_eq!(
            strings(&writer["tools"]["effective"]),
            vec!["workspace", "workspace.*", "composio"],
            "an agent that lists no tools holds the whole allow-list: {writer}"
        );
        assert_eq!(
            strings(&writer["tools"]["companyAllow"]),
            vec!["workspace", "workspace.*", "composio"],
            "the ceiling rides along, so a reader can tell an empty request \
             from an empty grant: {writer}"
        );

        // Desks, which are the graph's departments now: declared membership,
        // the lead flag off the effective order, and a stated empty list.
        let writer_desks = writer["desks"].as_array().unwrap();
        assert_eq!(writer_desks.len(), 1, "{writer}");
        assert_eq!(writer_desks[0]["id"], "content", "{writer}");
        assert_eq!(writer_desks[0]["name"], "Content desk", "{writer}");
        assert_eq!(writer_desks[0]["lead"], true, "{writer}");
        assert_eq!(ceo["desks"].as_array().unwrap()[0]["lead"], false, "{ceo}");
        assert!(
            row_of("hermit")["desks"].as_array().unwrap().is_empty(),
            "a teammate on no desk says so with an empty list rather than by \
             omitting the key: {roster}"
        );
        assert!(
            row_of(&jamie)["desks"].as_array().unwrap().is_empty(),
            "{roster}"
        );
    }

    /// An operator-added desk membership reaches the list, not just the detail
    /// read — otherwise the graph's pillars would go stale the moment somebody
    /// moved a teammate.
    #[tokio::test]
    async fn a_desk_change_shows_up_on_the_roster_list() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, _) = send(
            &state,
            "POST",
            "/api/v1/company/desks/content/members",
            Some(json!({"agent_id": "hermit"})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (_, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
        let hermit = roster
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "hermit")
            .unwrap()
            .clone();
        let desks = hermit["desks"].as_array().unwrap();
        assert_eq!(desks.len(), 1, "{hermit}");
        assert_eq!(desks[0]["id"], "content", "{hermit}");
    }

    /// A teammate created through the console reads back with the grant it
    /// actually holds, so the card the console renders from the POST response
    /// says the same thing the next list read will.
    #[tokio::test]
    async fn a_new_overlay_teammate_is_created_with_the_standard_grant() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, created) = send(
            &state,
            "POST",
            "/api/v1/company/team",
            Some(json!({"name": "Robin", "role": "Support"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(
            strings(&created["tools"]["effective"]),
            vec!["workspace", "workspace.*", "composio"],
            "{created}"
        );
        assert!(
            created["desks"].as_array().unwrap().is_empty(),
            "nobody has put it on a desk yet: {created}"
        );

        let (_, detail) = get_agent(&state, created["id"].as_str().unwrap()).await;
        assert_eq!(created["tools"], detail["tools"], "{created} vs {detail}");
        assert_eq!(created["desks"], detail["desks"], "{created} vs {detail}");
    }

    // --- The edit half ------------------------------------------------------

    /// The issue's "write-once per member", gone: a console-defined teammate can
    /// be corrected, and the correction is on the host rather than in a tab.
    #[tokio::test]
    async fn an_overlay_teammate_can_be_edited_and_the_edit_persists() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let jamie = add_overlay(&state, "Jamie", "Growth").await;

        let (status, edited) = patch_agent(
            &state,
            &jamie,
            json!({"name": "Jamie R", "role": "Head of Growth", "description": "Runs paid."}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{edited}");
        assert_eq!(edited["name"], "Jamie R", "{edited}");
        assert_eq!(edited["role"], "Head of Growth", "{edited}");
        assert_eq!(edited["description"], "Runs paid.", "{edited}");

        // Read back through a fresh request, so this is the stored record and
        // not the handler's own answer.
        let (_, reread) = get_agent(&state, &jamie).await;
        assert_eq!(reread["name"], "Jamie R", "{reread}");
        assert_eq!(reread["role"], "Head of Growth", "{reread}");

        // …and the roster list agrees, so the card the operator came from is
        // updated too rather than only the panel they edited in.
        let (_, roster) = send(&state, "GET", "/api/v1/company/team", None).await;
        let row = roster
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == jamie.as_str())
            .unwrap()
            .clone();
        assert_eq!(row["name"], "Jamie R", "{row}");
        assert_eq!(row["role"], "Head of Growth", "{row}");
    }

    /// A patch leaves what it does not mention alone, and an explicit `null`
    /// clears the description. Collapsing those two would make every partial
    /// save erase an agent's instructions.
    #[tokio::test]
    async fn an_absent_field_is_left_alone_and_null_clears_the_description() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let jamie = add_overlay(&state, "Jamie", "Growth").await;

        let (status, only_role) = patch_agent(&state, &jamie, json!({"role": "Growth Lead"})).await;
        assert_eq!(status, StatusCode::OK, "{only_role}");
        assert_eq!(only_role["name"], "Jamie", "{only_role}");
        assert_eq!(
            only_role["description"], "Original.",
            "an unmentioned field survives the patch: {only_role}"
        );

        let (status, cleared) = patch_agent(&state, &jamie, json!({"description": null})).await;
        assert_eq!(status, StatusCode::OK, "{cleared}");
        assert!(
            cleared["description"].is_null(),
            "an explicit null clears it: {cleared}"
        );
        assert_eq!(cleared["role"], "Growth Lead", "{cleared}");
    }

    /// The blueprint is not editable from a browser: a manifest teammate is a
    /// `409` naming where the edit belongs, and nothing is written.
    #[tokio::test]
    async fn a_manifest_teammate_cannot_be_edited_here() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, refusal) = patch_agent(&state, "ceo", json!({"role": "Chief Vibes"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refusal}");

        let (_, ceo) = get_agent(&state, "ceo").await;
        assert_eq!(ceo["role"], "Chief Executive", "{ceo}");
        assert!(
            ceo["editable"].as_array().unwrap().is_empty(),
            "and the host says so up front, so the console never offers the \
             field: {ceo}"
        );
    }

    /// The console renders read-only from the host's answer, not from a rule of
    /// its own — so this list is part of the contract.
    #[tokio::test]
    async fn the_host_states_which_fields_are_editable() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let jamie = add_overlay(&state, "Jamie", "Growth").await;

        let (_, agent) = get_agent(&state, &jamie).await;
        assert_eq!(
            strings(&agent["editable"]),
            vec!["name", "role", "description", "tools"],
            "{agent}"
        );
    }

    /// Issue #619: an overlay teammate can be narrowed, and the narrowing is
    /// what the host enforces — not a label beside an unchanged grant.
    ///
    /// The three levels are asserted separately on purpose. `requested` proves
    /// the scope was stored at all; `effective` proves it reached the function
    /// the harness builds the agent with; and the company `allow` staying put
    /// proves the narrowing is per-teammate rather than a company-wide edit.
    #[tokio::test]
    async fn an_overlay_teammate_can_be_scoped_and_the_scope_is_enforced() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let jamie = add_overlay(&state, "Jamie", "Growth").await;

        // Before: the widest grant the company has, which is the defect.
        let (_, before) = get_agent(&state, &jamie).await;
        assert!(
            strings(&before["tools"]["requested"]).is_empty(),
            "unscoped to begin with: {before}"
        );
        assert_eq!(
            strings(&before["tools"]["effective"]),
            vec!["workspace", "workspace.*", "composio"],
            "which resolves to everything the company allows: {before}"
        );

        let (status, scoped) = patch_agent(&state, &jamie, json!({"tools": ["workspace"]})).await;
        assert_eq!(status, StatusCode::OK, "{scoped}");
        assert_eq!(
            strings(&scoped["tools"]["requested"]),
            vec!["workspace"],
            "{scoped}"
        );
        assert_eq!(
            strings(&scoped["tools"]["effective"]),
            vec!["workspace"],
            "and it is narrower than the company grant, which is the whole \
             point: {scoped}"
        );
        assert_eq!(
            strings(&scoped["tools"]["companyAllow"]),
            vec!["workspace", "workspace.*", "composio"],
            "the company ceiling is untouched — this scoped one teammate, not \
             the company: {scoped}"
        );

        // Read back through a fresh request, so this is the stored record and
        // not the handler's own answer.
        let (_, reread) = get_agent(&state, &jamie).await;
        assert_eq!(
            strings(&reread["tools"]["requested"]),
            vec!["workspace"],
            "{reread}"
        );

        // An empty list is the deliberate way back to the standard grant, and
        // it must read as "inherits everything" rather than "holds nothing".
        let (status, cleared) = patch_agent(&state, &jamie, json!({"tools": []})).await;
        assert_eq!(status, StatusCode::OK, "{cleared}");
        assert!(
            strings(&cleared["tools"]["requested"]).is_empty(),
            "{cleared}"
        );
        assert_eq!(
            strings(&cleared["tools"]["effective"]),
            vec!["workspace", "workspace.*", "composio"],
            "{cleared}"
        );
    }

    /// A blank glob is refused rather than stored: `""` matches nothing an
    /// operator meant, so it would read as a scope that grants nothing while
    /// looking like a scope that was set.
    #[tokio::test]
    async fn a_blank_tool_glob_is_refused() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let jamie = add_overlay(&state, "Jamie", "Growth").await;

        let (status, refusal) =
            patch_agent(&state, &jamie, json!({"tools": ["workspace", "  "]})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");

        let (_, unchanged) = get_agent(&state, &jamie).await;
        assert!(
            strings(&unchanged["tools"]["requested"]).is_empty(),
            "and nothing was written: {unchanged}"
        );
    }

    /// A manifest teammate's tool line lives in the version-controlled
    /// blueprint, and #619 did not move it: the overlay half became writable,
    /// the manifest half stayed a `409`.
    #[tokio::test]
    async fn a_manifest_teammates_tools_are_still_not_editable_here() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, refusal) = patch_agent(&state, "ceo", json!({"tools": ["workspace"]})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
    }

    /// A blank name would render a card with no way back to it, so it is a
    /// refusal rather than a stored blank. Whitespace is trimmed, not accepted.
    #[tokio::test]
    async fn a_blank_name_or_role_is_refused() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;
        let jamie = add_overlay(&state, "Jamie", "Growth").await;

        for body in [json!({"name": "   "}), json!({"role": ""})] {
            let (status, refusal) = patch_agent(&state, &jamie, body.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} → {refusal}");
        }

        let (status, trimmed) = patch_agent(&state, &jamie, json!({"name": "  Jamie R  "})).await;
        assert_eq!(status, StatusCode::OK, "{trimmed}");
        assert_eq!(trimmed["name"], "Jamie R", "{trimmed}");
    }

    /// An id that names nobody is a `404` on both verbs, rather than a detail
    /// view of a teammate that does not exist or a write that lands nowhere.
    #[tokio::test]
    async fn an_unknown_teammate_is_not_found() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), ROSTER).await;

        let (status, _) = get_agent(&state, "nobody").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = patch_agent(&state, "nobody", json!({"role": "Ghost"})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
