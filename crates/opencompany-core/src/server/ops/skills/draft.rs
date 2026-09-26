//! `POST …/skills/draft` — one copilot turn on a skill document.
//!
//! The same contract as the teammate copilot's draft routes
//! (`docs/spec/runtime/api-team-drafting.md`): the body carries `messages`, the
//! answer is `{reply, text?}`, **nothing is written**, and the console owns the
//! transcript so the host stores none of it.
//!
//! # This route never writes
//!
//! It loads the company record, composes a prompt from it, and returns text.
//! The draft becomes a skill only if the operator takes it and saves it through
//! `POST …/skills`, which runs the validator and the scan like any other write.
//! Two deliberate human actions stand between this response and an agent's
//! prompt, and if either is ever removed this route has to be reconsidered with
//! it.
//!
//! # Who may ask
//!
//! Admin, matching the writes it feeds. Every skill write is admin-gated
//! because a skill decides something company-wide; a caller who could draft one
//! but not save it would only be able to spend the company's tokens.
//!
//! # The draft is scanned before it is shown
//!
//! A model writing a skill is untrusted text reaching a prompt like any other,
//! and the save path would refuse a document the scan blocks. Handing the
//! operator one anyway would mean a copilot whose output the Save button
//! rejects with no explanation, so a blocked draft withholds `text` and says
//! which findings blocked it.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, scoped};

/// Builds the skill-draft route fragment.
pub(super) fn router() -> Router<AppState> {
    scoped("/skills/draft", post(draft))
}

/// What the console asks for when it wants a skill drafted.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftSkillRequest {
    /// The conversation so far, oldest first — empty on the opening turn.
    ///
    /// The console holds the transcript and sends it back each turn; the host
    /// stores nothing. Free text from a stranger on both sides, and treated as
    /// such: framed to the model as a description of what the operator wants
    /// rather than as instructions to it, and bounded host-side.
    #[serde(default)]
    messages: Vec<WireTurn>,
}

/// One turn of a copilot conversation, on the wire.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTurn {
    /// `operator` or `copilot`. Anything else drops the turn.
    role: String,
    text: String,
}

/// One drafted skill, for the operator to keep or throw away.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillDraftDto {
    /// What the copilot says — what it wrote, or what it needs to know. Absent
    /// when the pass refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    reply: Option<String>,
    /// The whole `SKILL.md` as it now stands, never a diff. Absent on a turn
    /// that asked a question instead of drafting, and when the pass refused —
    /// `source` tells those apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// `model` when a model wrote this, `unavailable` when none could or the
    /// scan refused what it wrote.
    source: &'static str,
    /// Why there is no draft. Present only when `source` is `unavailable`, and
    /// distinct per cause because the operator's next move differs: wire up a
    /// model, retry the provider, say more, or rewrite what came back.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    /// What the scan said about the drafted document, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    scan: Option<super::ScanSummary>,
}

/// The reason a draft is withheld because the content scan refused it.
///
/// Not one of [`DraftRefusal`](crate::company::profile_draft::DraftRefusal)'s
/// four spellings on purpose: those describe why a model could not answer, and
/// this one describes an answer that arrived and was refused. The operator's
/// next move is different — say it differently, rather than wire something up.
const REFUSED_BY_SCAN: &str = "refused_by_scan";

/// `POST …/skills/draft` — draft a skill document (brief §6.4).
async fn draft(
    company: AdminScopedCompany,
    State(_state): State<AppState>,
    Json(body): Json<DraftSkillRequest>,
) -> Result<Json<SkillDraftDto>, ApiError> {
    let record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;

    let subject = crate::company::skill_draft::SkillSubject {
        company_name: record.manifest.company.name.clone(),
        company_output: record.manifest.company.output.clone(),
        conversation: crate::company::profile_draft::clamp_conversation(
            body.messages
                .into_iter()
                .filter_map(|turn| {
                    crate::company::profile_draft::TurnRole::parse(&turn.role).map(|role| {
                        crate::company::profile_draft::CopilotTurn {
                            role,
                            text: turn.text,
                        }
                    })
                })
                .collect(),
        ),
    };

    let drafted = build_draft(&company, &record, &subject).await;
    tracing::info!(
        company = %company.id(),
        outcome = drafted.refusal().map(|r| r.as_str()).unwrap_or("drafted"),
        "[skill-draft] answered a skill draft request"
    );
    Ok(Json(vet(drafted)))
}

/// Runs the drafted document through the same validator and scan a save would,
/// and withholds it when the scan blocks.
///
/// The slug is derived the way `POST …/skills` would derive it from the same
/// document, so the validation the operator would meet on Save is the
/// validation run here rather than an approximation of it.
fn vet(drafted: crate::company::skill_draft::SkillDraft) -> SkillDraftDto {
    use crate::company::skill_draft::SkillDraft;

    let (reply, doc) = match drafted {
        SkillDraft::Refused(reason) => {
            return SkillDraftDto {
                reply: None,
                text: None,
                source: "unavailable",
                reason: Some(reason.as_str()),
                scan: None,
            };
        }
        SkillDraft::Answered { reply, doc } => (reply, doc),
    };

    let Some(doc) = doc else {
        return SkillDraftDto {
            reply: Some(reply),
            text: None,
            source: "model",
            reason: None,
            scan: None,
        };
    };

    let slug = slug_for(&doc);
    match super::vet_skill(&slug, &doc, false) {
        Ok(scan) => SkillDraftDto {
            reply: Some(reply),
            text: Some(doc),
            source: "model",
            reason: None,
            scan: Some(scan),
        },
        Err(problem) => SkillDraftDto {
            reply: Some(format!(
                "{reply}\n\nThat draft was refused before it reached you: {}",
                problem.message()
            )),
            text: None,
            source: "unavailable",
            // A document the scan blocked and one that never validated are
            // different answers to "what do I do now": say it differently, or
            // write it by hand. Reporting both as a scan refusal told the
            // operator to reword a draft the scan had never objected to.
            reason: Some(if problem.is_scan_block() {
                REFUSED_BY_SCAN
            } else {
                crate::company::profile_draft::DraftRefusal::Unreadable.as_str()
            }),
            scan: None,
        },
    }
}

/// The slug a drafted document would be stored under.
fn slug_for(doc: &str) -> String {
    use crate::company::skill_validate::slugify;
    match crate::company::parse_skill_md("draft", doc) {
        Ok(parsed) => slugify(&parsed.name),
        // An unparseable document has no name to slug; `vet_skill` is about to
        // report exactly why it is unparseable, and a placeholder slug keeps
        // that report about the document rather than about the slug.
        Err(_) => "draft".to_string(),
    }
}

/// Runs the drafting pass, reserving its ceiling first.
///
/// The same order and the same reasoning as the teammate copilot's own pass: a
/// company with nothing wired has a truer answer than "out of budget", and a
/// company that is out of budget must not reach the provider at all.
#[cfg(feature = "openhuman")]
async fn build_draft(
    company: &AdminScopedCompany,
    record: &crate::ports::types::CompanyRecord,
    subject: &crate::company::skill_draft::SkillSubject,
) -> crate::company::skill_draft::SkillDraft {
    use crate::company::profile_draft::DraftRefusal;
    use crate::company::skill_draft::SkillDraft;

    // The same drafter `GET …/inference` reports as `designsProfiles`, so the
    // control the console offers and the capability this route needs cannot
    // disagree.
    let Some(drafter) = company.runtime.profile_drafter() else {
        return SkillDraft::Refused(DraftRefusal::NoModel);
    };
    let Some(_budget) = crate::server::ops::team_agent::reserve_draft_budget(
        company.id(),
        company.runtime.usage().as_ref(),
        &record.manifest.plan,
        crate::harness::skill_draft::skill_output_ceiling(),
    )
    .await
    else {
        return SkillDraft::Refused(DraftRefusal::BudgetExhausted);
    };
    let provider = drafter.provider_slug();
    let (drafted, usage) = crate::harness::skill_draft::draft_skill(&drafter, subject).await;
    let model = drafter.model_slug();
    crate::metering::record_profile_draft_usage(
        &usage,
        &provider,
        model,
        company.id(),
        company.runtime.store().as_ref(),
        company.runtime.usage().as_ref(),
    )
    .await;
    drafted
}

/// The default build links no harness, so there is no model to draft with and
/// saying so is the whole answer.
#[cfg(not(feature = "openhuman"))]
async fn build_draft(
    _company: &AdminScopedCompany,
    _record: &crate::ports::types::CompanyRecord,
    _subject: &crate::company::skill_draft::SkillSubject,
) -> crate::company::skill_draft::SkillDraft {
    crate::company::skill_draft::SkillDraft::Refused(
        crate::company::profile_draft::DraftRefusal::NoModel,
    )
}

#[cfg(test)]
#[path = "draft_tests.rs"]
mod tests;
