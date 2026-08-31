//! `PATCH {scope}` — the conscious naming step of the account-activation
//! funnel (issue #1844): sets the company's display name and stamps
//! [`CompanyRecord::name_confirmed`], the first of the three activation steps
//! [`crate::company::activation`] derives.
//!
//! ## Why this writes the manifest directly, unlike every other console write
//!
//! Every other operator write in `ops` lands in an overlay field on
//! [`CompanyRecord`] — never `record.manifest` — because a rebuild
//! re-persists the manifest from the seed on every boot (`RuntimeBuilder::build`),
//! which would silently wipe a direct manifest write on the next redeploy. This
//! route is the one deliberate exception: the display name genuinely **is**
//! `[company].name`, the same field `company.toml` seeds, so there is no
//! separate "confirmed name" value to keep in an overlay — writing the
//! manifest field IS the write. What makes this safe rather than the same trap
//! every overlay doc warns about is `RuntimeBuilder::build`'s own carry-forward
//! (issue #1844, beside `merge_enabled_workflows`): once `name_confirmed` is
//! `true`, a rebuild copies the *existing record's* name back onto the freshly
//! parsed seed manifest before either is used, exactly the way `[workflows].enabled`
//! is merged instead of re-derived. Before confirmation, no such carry exists —
//! the seed name is a provisional default and stays seed-authoritative, which is
//! the correct behaviour for an operator still editing `company.toml` pre-launch.
//!
//! ## Attribution and authority
//!
//! Admin-only, like the sibling [`policy`](super::policy) and per-teammate
//! budget writes — renaming the company's own identity is at least as sharp a
//! boundary as either. [`ScopedCompany`] resolves addressing and the
//! temporary-password gate; [`require_admin`] adds the authority check.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::patch;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::store::company_write_lock;
use crate::ports::types::{CompanyEvent, CompanyRecord, OnboardingStep};
use crate::server::error::ApiError;
use crate::server::graphql::auth::MaybePeer;
use crate::server::ops::{ScopedCompany, scoped};
use crate::server::users::admin::require_admin;

/// Builds the company-profile route fragment.
pub fn router() -> Router<AppState> {
    scoped("", patch(patch_company))
}

/// The `PATCH {scope}` request body. `name` is the only field this route
/// accepts today — a company's identity has exactly one console-writable
/// piece, and a body that named anything else would be silently ignored,
/// which is worse than a route that simply does not exist yet for it.
#[derive(Debug, Deserialize)]
struct PatchCompanyInput {
    #[serde(default)]
    name: Option<String>,
}

/// Max length of the company's display name, matching the convention set by
/// `MAX_WORKFLOW_NAME_LEN` (`src/company/workflow_create.rs`).
///
/// PR #1875 review finding: this name is embedded verbatim into every
/// agent's system prompt (`persona_prompt`, `src/company/prompt.rs`), with no
/// bound before this — an accidental pasted document is enough to inflate
/// every model request until context limits are exceeded and workflows stop
/// running.
const COMPANY_NAME_MAX_CHARS: usize = 200;

/// What the console gets back after a successful rename.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PatchCompanyDto {
    name: String,
    name_confirmed: bool,
}

fn refusal(message: &str) -> Response {
    (
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        message.to_string(),
    )
        .into_response()
}

/// `PATCH {scope}` — set the company's display name and stamp
/// [`CompanyRecord::name_confirmed`].
///
/// Idempotent in the sense that matters for its own contract: renaming an
/// already-confirmed company (any operator, any time — this is an ordinary
/// rename, not only the first-run step) still succeeds and keeps
/// `name_confirmed` set. What is *not* re-emitted on a later rename is the
/// [`CompanyEvent::OnboardingStepCompleted`] audit line — see the `first`
/// guard below — because that event marks the funnel step's first completion,
/// not every subsequent edit.
async fn patch_company(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    MaybePeer(peer): MaybePeer,
    Json(body): Json<PatchCompanyInput>,
) -> Result<Json<PatchCompanyDto>, crate::server::Rejection> {
    require_admin(&headers, &state, &company.runtime, peer).await?;

    let Some(name) = body.name else {
        return Err(refusal("Nothing to set. Send `name`.").into());
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        // Same rule and the same words `CompanyManifest::validate` uses for
        // `[company].name` — a console rejection and a `company.toml`
        // rejection must not describe the same requirement differently.
        return Err(refusal("`name` cannot be empty — give your company a name.").into());
    }
    if name.chars().count() > COMPANY_NAME_MAX_CHARS {
        return Err(refusal(&format!(
            "a company name can be at most {COMPANY_NAME_MAX_CHARS} characters."
        ))
        .into());
    }

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;
    let first = !record.name_confirmed;
    record.manifest.company.name = name.clone();
    record.name_confirmed = true;

    company.runtime.store().save(&record).await?;

    // Only the transition into confirmation is journaled — see the handler's
    // own doc comment. Best-effort: the record write above already landed, so
    // a journal failure here never leaves the name unset, only the audit
    // trail thinner (the same trade-off `compute_and_latch` makes for
    // `OnboardingCompleted`).
    if first
        && let Err(err) = company
            .runtime
            .events()
            .append(
                company.id(),
                CompanyEvent::OnboardingStepCompleted {
                    step: OnboardingStep::NameConfirmed,
                },
            )
            .await
    {
        tracing::warn!(
            company = %company.id(),
            %err,
            "company confirmed its name but the OnboardingStepCompleted audit event could not be journaled"
        );
    }

    Ok(Json(PatchCompanyDto {
        name: record.manifest.company.name,
        name_confirmed: record.name_confirmed,
    }))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, EventSeq};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::{AppConfig, AppState};

    const MANIFEST: &str = "[company]\nname = \"Provisional Co\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-company-profile-")
            .tempdir()
            .expect("tempdir")
    }

    /// Builds straight from an empty store — no pre-seeded [`CompanyRecord`] —
    /// so this is a genuinely first-ever boot from `RuntimeBuilder::build`'s
    /// own point of view (`existing: None`).
    ///
    /// A pre-seed-then-build pattern (as `ops::policy`'s own tests use, which
    /// this used to copy) makes `existing: Some(record with lifecycle:
    /// "running", activation_completed_at: None)` true from the very first
    /// `.build()` call — the exact shape the "running and unlatched"
    /// grandfather back-fill (issue #1843) matches, which stamps
    /// `name_confirmed: true` immediately regardless of what the pre-seeded
    /// record actually said. That is fine for `policy`'s tests, which never
    /// assert on `name_confirmed`; it silently defeats every assertion here,
    /// which is exactly what `name_confirmed` starting `false` is supposed to
    /// prove. Building with nothing pre-seeded is the one shape the migration
    /// does not grandfather (see `RuntimeBuilder::build`'s own `None => (false,
    /// None)` arm), so `name_confirmed` starts `false` as this module's tests
    /// need it to.
    async fn state(home: &std::path::Path) -> AppState {
        let manifest: CompanyManifest = toml::from_str(MANIFEST).unwrap();
        let id = CompanyId::new("acme");
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

    async fn call(state: &AppState, name: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("PATCH")
            .uri("/api/v1/company")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json")
            .body(Body::from(json!({ "name": name }).to_string()))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn empty_name_is_refused() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, _) = call(&state, "   ").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let id = CompanyId::new("acme");
        let store = state.registry().get(&id).unwrap().store().clone();
        let reloaded = store.load(&id).await.unwrap().unwrap();
        assert!(
            !reloaded.name_confirmed,
            "a refused write must not stamp the flag"
        );
    }

    /// PR #1875 review finding: an unbounded name is embedded verbatim into
    /// every agent's system prompt (`persona_prompt`,
    /// `src/company/prompt.rs`), so one oversized paste (an accidental
    /// pasted document is enough) inflates every model request until
    /// context limits are exceeded and workflows stop running.
    #[tokio::test]
    async fn an_oversized_name_is_refused() {
        let dir = home();
        let state = state(dir.path()).await;
        let too_long = "x".repeat(COMPANY_NAME_MAX_CHARS + 1);
        let (status, _) = call(&state, &too_long).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let id = CompanyId::new("acme");
        let store = state.registry().get(&id).unwrap().store().clone();
        let reloaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(
            reloaded.manifest.company.name, "Provisional Co",
            "a refused oversized name must not be persisted"
        );
        assert!(
            !reloaded.name_confirmed,
            "a refused write must not stamp the flag"
        );
    }

    /// A name sitting exactly at the limit is still an ordinary rename.
    #[tokio::test]
    async fn a_name_at_the_limit_is_accepted() {
        let dir = home();
        let state = state(dir.path()).await;
        let at_limit = "x".repeat(COMPANY_NAME_MAX_CHARS);
        let (status, body) = call(&state, &at_limit).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], at_limit);
    }

    /// PR #1875 review finding: the limit must count characters, not UTF-8
    /// bytes. `COMPANY_NAME_MAX_CHARS` (200) is what the API error message
    /// and the console `maxLength` both advertise to the operator, so a
    /// name made of 200 multibyte characters — well within what both surfaces
    /// promise — must be accepted even though it is 600 bytes on the wire.
    #[tokio::test]
    async fn a_multibyte_name_at_the_char_limit_is_accepted() {
        let dir = home();
        let state = state(dir.path()).await;
        // "あ" is 3 UTF-8 bytes; 200 of them is 200 chars / 600 bytes.
        let at_limit = "あ".repeat(COMPANY_NAME_MAX_CHARS);
        let (status, body) = call(&state, &at_limit).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], at_limit);
    }

    /// The same limit still refuses a name one character past it, even when
    /// every character is multibyte.
    #[tokio::test]
    async fn a_multibyte_name_over_the_char_limit_is_refused() {
        let dir = home();
        let state = state(dir.path()).await;
        let over_limit = "あ".repeat(COMPANY_NAME_MAX_CHARS + 1);
        let (status, _) = call(&state, &over_limit).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn sets_name_and_stamps_the_flag() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "  Real Name  ").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "Real Name");
        assert_eq!(body["nameConfirmed"], true);

        let id = CompanyId::new("acme");
        let store = state.registry().get(&id).unwrap().store().clone();
        let reloaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(reloaded.manifest.company.name, "Real Name");
        assert!(reloaded.name_confirmed);
    }

    #[tokio::test]
    async fn the_step_event_is_journaled_only_once() {
        let dir = home();
        let state = state(dir.path()).await;
        let id = CompanyId::new("acme");
        let events = state.registry().get(&id).unwrap().events().clone();

        for name in ["First Name", "Second Name"] {
            let (status, _) = call(&state, name).await;
            assert_eq!(status, StatusCode::OK);
        }

        let stored = events
            .read_from(&id, EventSeq::new(0), usize::MAX)
            .await
            .unwrap();
        let step_events = stored
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.event,
                    CompanyEvent::OnboardingStepCompleted {
                        step: OnboardingStep::NameConfirmed
                    }
                )
            })
            .count();
        assert_eq!(step_events, 1, "two renames must journal the step once");
    }
}
