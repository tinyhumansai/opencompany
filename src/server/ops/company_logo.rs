//! Company-logo settings, persisted in the company manifest.

use axum::routing::put;
use axum::{Json, Router};
use serde::Deserialize;

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::store::company_write_lock;
use crate::runtime::CompanyStatus;
use crate::server::error::ApiError;
use crate::server::ops::{AdminScopedCompany, scoped};

const COMPANY_LOGO_MAX_CHARS: usize = 1_000_000;
const ALLOWED_IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompanyLogoBody {
    #[serde(default)]
    logo_url: Option<String>,
}

/// Builds both `PUT /api/v1/company/logo` addressing variants.
pub fn router() -> Router<AppState> {
    scoped("/logo", put(put_logo))
}

fn invalid_logo(message: impl Into<String>) -> ApiError {
    ApiError(OpenCompanyError::InvalidRequest(message.into()))
}

/// Accepts only bounded, self-contained image data URLs. `None` clears the logo.
fn company_logo_value(value: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.len() > COMPANY_LOGO_MAX_CHARS {
        return Err(invalid_logo(format!(
            "company logo exceeds the {COMPANY_LOGO_MAX_CHARS}-character limit"
        )));
    }

    let (header, payload) = value
        .split_once(',')
        .ok_or_else(|| invalid_logo("company logo must be a base64 image data URL"))?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|header| header.strip_suffix(";base64"))
        .filter(|mime| ALLOWED_IMAGE_MIMES.contains(mime))
        .ok_or_else(|| {
            invalid_logo("company logo must be a base64 PNG, JPEG, GIF, or WebP data URL")
        })?;
    let padding = payload.len() - payload.trim_end_matches('=').len();
    if payload.is_empty()
        || payload.len() % 4 != 0
        || padding > 2
        || !payload[..payload.len() - padding]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
    {
        return Err(invalid_logo(format!(
            "company logo contains invalid base64 data for {mime}"
        )));
    }

    Ok(Some(value))
}

/// `PUT …/logo` — replace or clear the company logo and return fresh status.
///
/// PR #1875 review finding: this is a load-modify-save cycle over the whole
/// [`CompanyRecord`] manifest, exactly the shape `company_write_lock` exists
/// to serialize (see its own doc comment) — every sibling console write
/// (`company_profile::patch_company`, `team.rs`, `team_agent.rs`,
/// `tool_grants.rs`, `policy.rs`, `setup.rs`) already takes it. This one did
/// not, so a rename landing between this handler's `load` and `save` was
/// silently reverted: this handler's `save` writes back the whole manifest it
/// loaded, including the pre-rename `name`, even though it only meant to
/// change `logo_url`.
async fn put_logo(
    company: AdminScopedCompany,
    Json(body): Json<CompanyLogoBody>,
) -> Result<Json<CompanyStatus>, ApiError> {
    let logo_url = company_logo_value(body.logo_url)?;

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| OpenCompanyError::CompanyNotFound(company.id().to_string()))?;
    record.manifest.company.logo_url = logo_url;
    company.runtime.store().save(&record).await?;
    Ok(Json(company.runtime.status().await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_value_accepts_images_and_rejects_external_or_oversized_values() {
        let valid = "data:image/png;base64,iVBORw==".to_string();
        assert_eq!(
            company_logo_value(Some(valid.clone())).unwrap(),
            Some(valid)
        );
        assert!(company_logo_value(Some("https://example.com/logo.png".into())).is_err());
        assert!(company_logo_value(Some("x".repeat(COMPANY_LOGO_MAX_CHARS + 1))).is_err());
    }

    // ------------------------------------------------------------------
    // PR #1875 review finding: `put_logo`'s load-modify-save cycle did not
    // take `company_write_lock`, unlike every sibling console write
    // (`company_profile::patch_company`, `team.rs`, `team_agent.rs`,
    // `tool_grants.rs`, `policy.rs`, `setup.rs`). A rename that lands between
    // this handler's `load` and `save` is silently overwritten — the load
    // already has the pre-rename manifest, and this handler's `save` writes
    // that whole manifest back, including `name` and `name_confirmed`, even
    // though this handler never touched either field.
    // ------------------------------------------------------------------

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::CompanyId;
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::{AppConfig, AppState};

    const MANIFEST: &str = "[company]\nname = \"Provisional Co\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-company-logo-")
            .tempdir()
            .expect("tempdir")
    }

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

    async fn put_logo_request(state: &AppState, logo_url: &str) -> StatusCode {
        let request = Request::builder()
            .method("PUT")
            .uri("/api/v1/company/logo")
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "logoUrl": logo_url }).to_string(),
            ))
            .unwrap();
        router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    /// `put_logo` must now serialize against `company_write_lock`, exactly
    /// like every other load-modify-save console write. Proven the same way
    /// `assert_delete_label_survives_a_concurrent_identical_put` proves
    /// serialisation for the store layer (`store/conformance.rs`): hold the
    /// lock externally, drive the real handler through the router, and
    /// demand it cannot finish while the lock is held.
    #[tokio::test]
    async fn put_logo_serializes_against_the_company_write_lock() {
        let dir = home();
        let state = state(dir.path()).await;
        let id = CompanyId::new("acme");

        let lock = crate::ports::store::company_write_lock(&id);
        let guard = lock.lock().await;

        let state_for_task = state.clone();
        let mut task = tokio::spawn(async move {
            put_logo_request(&state_for_task, "data:image/png;base64,AA==").await
        });

        // The handler must be blocked behind the held lock — give it every
        // chance to (wrongly) race ahead before declaring it stuck.
        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "put_logo completed while company_write_lock was held elsewhere — \
             it is not serializing its load-modify-save cycle against \
             concurrent writers"
        );

        drop(guard);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("put_logo never resumed after the lock was released")
            .expect("put_logo task panicked");
        assert_eq!(status, StatusCode::OK);
    }

    /// The concrete lost-update this closes: a rename and a logo write
    /// racing for real must never both land with only one surviving — the
    /// #1875 bug was `put_logo`'s unlocked load-modify-save silently
    /// reverting whichever rename happened to complete in its window.
    ///
    /// Driven exactly like `assert_delete_label_survives_a_concurrent_identical_put`
    /// (`store/conformance.rs`): both writers are spawned rather than
    /// awaited in order, so they interleave at every `.await` even on a
    /// current-thread runtime, and run for many rounds since any single
    /// round can luck into an ordering that does not exercise the race. If
    /// both writes share `company_write_lock`, whichever one runs second
    /// necessarily starts from a fresh load of the one that ran first — so
    /// both changes survive regardless of which order the scheduler picks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn put_logo_and_rename_do_not_lose_each_others_write() {
        let dir = home();
        let state = state(dir.path()).await;
        let id = CompanyId::new("acme");
        let store = state.registry().get(&id).unwrap().store().clone();

        for round in 0..32 {
            let name = format!("Renamed Co {round}");
            let logo = format!("data:image/png;base64,{round:0>4}AA==");

            let state_a = state.clone();
            let name_for_task = name.clone();
            let rename_task = tokio::spawn(async move {
                let request = Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/company")
                    .header("cookie", crate::server::test_support::fixed_cookie("acme"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "name": name_for_task }).to_string(),
                    ))
                    .unwrap();
                router(state_a.clone())
                    .oneshot(request)
                    .await
                    .unwrap()
                    .status()
            });

            let state_b = state.clone();
            let logo_for_task = logo.clone();
            let logo_task =
                tokio::spawn(async move { put_logo_request(&state_b, &logo_for_task).await });

            let (rename_status, logo_status) = tokio::join!(rename_task, logo_task);
            assert_eq!(rename_status.unwrap(), StatusCode::OK);
            assert_eq!(logo_status.unwrap(), StatusCode::OK);

            let reloaded = store.load(&id).await.unwrap().unwrap();
            assert_eq!(
                reloaded.manifest.company.name, name,
                "round {round}: a concurrent put_logo save reverted the \
                 rename that raced it"
            );
            assert!(
                reloaded.name_confirmed,
                "round {round}: a concurrent put_logo save reverted \
                 name_confirmed back to its pre-rename value"
            );
            assert_eq!(
                reloaded.manifest.company.logo_url,
                Some(logo),
                "round {round}: a concurrent rename save reverted the logo \
                 write that raced it"
            );
        }
    }
}
