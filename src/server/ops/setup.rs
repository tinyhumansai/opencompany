//! First-run company setup: propose a starting roster from three answers
//! (`docs/spec/runtime/company-setup.md`).
//!
//! `POST {scope}/setup/roster` takes what the operator said about their business
//! and returns four to six agents to create. It is the only new host surface
//! Phase 1 needs.
//!
//! ## It proposes; it does not create
//!
//! This route writes no teammates. The console takes the returned rows and
//! creates each one through the existing `POST {scope}/team`
//! ([`super::team`]), which buys three things worth more than the round trip:
//!
//! * **The build-out screen has something to show.** Each agent appears as its
//!   own write lands, which is the moment the whole feature is designed around.
//!   A single atomic create would finish invisibly and need an event channel to
//!   narrate itself.
//! * **One creation path.** A teammate made by setup is byte-identical to one an
//!   operator adds by hand, because it *is* one — no second write path to drift.
//! * **A half-finished setup is not a broken company.** If the browser closes
//!   after three creates, the company has three real teammates and an empty
//!   roster check that still offers setup for the rest.
//!
//! ## What it does write
//!
//! The answers, onto [`CompanyRecord::setup`](crate::ports::types::CompanyRecord).
//! Phase 2 builds this company's workflows from them, and asking a second time
//! would undo the thing setup exists to buy. They are stored even if the
//! operator then abandons the flow — see the field's own note on why that cannot
//! suppress a later offer.
//!
//! ## Any member may run it
//!
//! [`ScopedCompany`], not [`AdminScopedCompany`](super::scope::AdminScopedCompany).
//! `src/server/ops/scope.rs` is explicit that adding a teammate is deliberately
//! open to any member; this route proposes exactly that and then feeds the route
//! which performs it, so a stricter guard here would be a boundary that the very
//! next call does not enforce.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::setup::{
    FallbackReason, ProposedAgent, RosterProposal, SetupAnswers, template_proposal,
};
use crate::error::OpenCompanyError;
use crate::ports::store::company_write_lock;
use crate::ports::types::CompanyRecord;
use crate::server::error::ApiError;
use crate::server::ops::scope::{ScopedCompany, scoped};

/// Registers the setup route fragment under both addressing forms.
pub fn router() -> Router<AppState> {
    scoped("/setup/roster", post(propose_roster))
}

/// The three answers, as the console sends them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupRequest {
    /// "What kind of company are you setting up?"
    #[serde(default)]
    industry: String,
    /// "What team do you need?"
    #[serde(default)]
    team_hint: String,
    /// "What are you trying to automate?"
    #[serde(default)]
    automate: String,
}

impl From<SetupRequest> for SetupAnswers {
    fn from(body: SetupRequest) -> Self {
        Self {
            industry: body.industry,
            team_hint: body.team_hint,
            automate: body.automate,
        }
    }
}

/// One proposed teammate, shaped for the console's `addTeamMember` call so the
/// build-out step can pass a row straight through.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProposedAgentDto {
    name: String,
    role: String,
    description: String,
    /// The job shape that decides this teammate's tool belt.
    focus: Option<String>,
}

impl From<ProposedAgent> for ProposedAgentDto {
    fn from(agent: ProposedAgent) -> Self {
        Self {
            name: agent.name,
            role: agent.role,
            description: agent.description,
            focus: agent.focus.map(|f| f.as_str().to_string()),
        }
    }
}

/// The proposal.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RosterProposalDto {
    agents: Vec<ProposedAgentDto>,
    /// Which curated roster framed this proposal, e.g. `ecommerce`.
    template: String,
    /// Who wrote this team: `model` or `fallback`.
    ///
    /// The console **says which**. An earlier version reported a `generated`
    /// boolean and rendered both cases identically, on the theory that to an
    /// operator they are the same thing — a starting point they can edit. That
    /// was wrong in the one direction that matters: someone shown a canned team
    /// with no indication believes a model read their answers and produced it,
    /// and judges the product on a roster it never wrote.
    source: String,
    /// The jobs the operator named, as the host split them.
    jobs: Vec<String>,
    /// The jobs no teammate owns — non-empty only on the `model` path.
    uncovered: Vec<String>,
    /// Why this is the curated team: `no_model`, `model_unreachable` or
    /// `not_designable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl From<RosterProposal> for RosterProposalDto {
    fn from(proposal: RosterProposal) -> Self {
        Self {
            agents: proposal.agents.into_iter().map(Into::into).collect(),
            template: proposal.template_key.to_string(),
            source: proposal.source.as_str().to_string(),
            jobs: proposal.jobs,
            uncovered: proposal.uncovered,
            reason: proposal.reason.map(|r| r.as_str()),
        }
    }
}

/// `POST {scope}/setup/roster` — propose a starting roster.
async fn propose_roster(
    company: ScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<SetupRequest>,
) -> Result<Json<RosterProposalDto>, crate::server::Rejection> {
    let answers: SetupAnswers = body.into();

    // Remember the answers first. The proposal below may take seconds and the
    // operator may close the tab during it; what they told us about their
    // business is worth keeping either way, and Phase 2 needs it.
    store_answers(&company, &answers).await?;

    let proposal = build_proposal(&company, &answers).await;
    // Logged on the way out, including the happy path.
    //
    // Until this line the only `tracing` calls on this route were on failure
    // arms, so a setup that worked left no trace at all: reconstructing what a
    // company had been given meant reading `usage.jsonl` and `meta.json` by
    // hand. This is the first thing anyone asks about a first-run flow, so it
    // belongs in the log — which template framed it, whether a model designed
    // the team, and how many agents the operator is about to be shown.
    tracing::info!(
        company = %company.id(),
        template = proposal.template_key,
        source = proposal.source.as_str(),
        agents = proposal.agents.len(),
        "[setup] proposed a starting roster"
    );
    Ok(Json(proposal.into()))
}

/// Persists the answers onto the company record.
///
/// Takes the per-company write lock for the same reason [`super::team`]'s add
/// does: this is a read-modify-write of the whole record, and a concurrent
/// `POST …/team` from the build-out step of a *previous* attempt would otherwise
/// lose one of the two writes.
async fn store_answers(
    company: &ScopedCompany,
    answers: &SetupAnswers,
) -> Result<(), crate::server::Rejection> {
    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(company).await?;
    record.setup = Some(answers.clone());
    company
        .runtime
        .store()
        .save(&record)
        .await
        .map_err(|e| ApiError(e).into_response().into())
}

/// The proposal itself: designed by the model when one is wired, the curated
/// reference team otherwise.
///
/// The two arms are not a happy path and a degraded one. A company with no
/// inference credential is a supported configuration, and the template it gets
/// is a real industry roster — see `crate::company::setup`.
#[cfg(feature = "openhuman")]
async fn build_proposal(company: &ScopedCompany, answers: &SetupAnswers) -> RosterProposal {
    let Some(builder) = company.runtime.roster_builder().cloned() else {
        return template_proposal(answers, FallbackReason::NoModel);
    };
    let provider = builder.provider_slug();
    let (proposal, usage) = builder.propose(answers).await;
    // Read *after* the pass, so it names the model the pass actually ran on.
    let model = builder.model_slug();
    // Metered after the fact and never in the way: the pass has already produced
    // the roster the operator is about to see, and a meter write must not be
    // able to fail it.
    crate::metering::roster_build::record_roster_build_usage(
        &usage,
        &provider,
        model,
        company.id(),
        company.runtime.store().as_ref(),
        company.runtime.usage().as_ref(),
    )
    .await;
    proposal
}

/// The default build has no harness, so there is no model to polish with. The
/// curated template is the whole answer, and it is a good one.
#[cfg(not(feature = "openhuman"))]
async fn build_proposal(_company: &ScopedCompany, answers: &SetupAnswers) -> RosterProposal {
    template_proposal(answers, FallbackReason::NoModel)
}

/// Loads the addressed company's record, or 404s.
async fn load_record(company: &ScopedCompany) -> Result<CompanyRecord, crate::server::Rejection> {
    company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(company.id().to_string()))
                .into_response()
                .into()
        })
}
