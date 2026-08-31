//! Per-person channel read markers: `GET`/`PUT {scope}/chat/read-state`
//! (issue #755).
//!
//! Unread used to be decided entirely in the browser, against a floor stamped
//! when the tab mounted. Everything older than that instant counted as read, so
//! a reload marked every channel caught up and two tabs of the same person
//! disagreed. This is the durable half: where each person has read to, per
//! channel, remembered by the host.
//!
//! **The count stays in the console.** Only the browser holds the transcript,
//! so only the browser can say how many messages sit past a marker. What moves
//! here is the *floor* that count is measured from — which is the part that has
//! to outlive the tab. Splitting it the other way would mean shipping every
//! channel's message count to the host on every poll.
//!
//! **Signed-in humans only.** A marker answers "how far has *this person*
//! read", and a machine credential is not a person — the platform scope reaches
//! these routes with [`ScopedCompany::actor`] as `None`. Rather than invent a
//! shared pseudo-user for it (which would let one tenant's automation clear a
//! real operator's badges), both routes answer `401` for a caller with no
//! person behind it.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::ports::read_state::ChannelRead;
use crate::server::ops::scope::{ScopedCompany, scoped};

pub fn router() -> Router<AppState> {
    scoped("/chat/read-state", get(list_read_state).put(mark_read))
}

/// One channel's marker, as the console reads it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReadMarkerDto {
    channel_id: String,
    last_read_at: i64,
}

impl From<ChannelRead> for ReadMarkerDto {
    fn from(r: ChannelRead) -> Self {
        Self {
            channel_id: r.channel_id,
            last_read_at: r.last_read_at,
        }
    }
}

/// `GET {scope}/chat/read-state` — every marker this person holds.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReadStateDto {
    /// Channels this person has opened, each with its floor.
    ///
    /// A channel absent from this list has never been opened by this person.
    /// The console decides what that means — it is the only side that knows
    /// whether the channel even has messages — rather than the host guessing a
    /// zero that would render a lifetime of history as unread.
    markers: Vec<ReadMarkerDto>,
}

/// `PUT {scope}/chat/read-state` — move one channel's floor forward.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarkReadBody {
    channel_id: String,
    /// Milliseconds since the epoch, from the newest row the person has seen.
    last_read_at: i64,
}

async fn list_read_state(
    company: ScopedCompany,
) -> Result<Json<ReadStateDto>, crate::server::Rejection> {
    let Some(user) = actor_id(&company) else {
        return Err(unauthorized().into());
    };
    let markers = company
        .runtime
        .read_state()
        .list(company.id(), &user)
        .await?
        .into_iter()
        .map(ReadMarkerDto::from)
        .collect();
    Ok(Json(ReadStateDto { markers }))
}

async fn mark_read(
    company: ScopedCompany,
    Json(body): Json<MarkReadBody>,
) -> Result<Json<ReadMarkerDto>, crate::server::Rejection> {
    let Some(user) = actor_id(&company) else {
        return Err(unauthorized().into());
    };
    if body.channel_id.trim().is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "channelId must not be empty", "code": "invalid_request" })),
        )
            .into_response()
            .into());
    }
    // The stored marker is returned rather than the requested one, because
    // `mark` is monotonic: a late request carrying an earlier instant leaves the
    // marker where it was, and the console must see where it actually stands
    // rather than assume its own value took.
    let settled = company
        .runtime
        .read_state()
        .mark(company.id(), &user, &body.channel_id, body.last_read_at)
        .await?;
    Ok(Json(settled.into()))
}

/// The signed-in person behind the request, if there is one.
///
/// `Option` rather than `Result<_, Response>`: an axum `Response` is a large
/// error variant to carry through a helper, and the two call sites want the
/// same `401` anyway — so they build it once, from [`unauthorized`].
fn actor_id(company: &ScopedCompany) -> Option<String> {
    company.actor.as_ref().map(|a| a.id.clone())
}

/// The `401` for a caller with no person behind it.
///
/// See the module note: a machine credential has no person to attribute a
/// marker to, and inventing one would let automation clear a human's badges.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "read state is per person, and this credential names none",
            "code": "unauthorized",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    const MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-read-state-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state(home: &std::path::Path) -> AppState {
        let manifest: CompanyManifest = toml::from_str(MANIFEST).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
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
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
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

    /// A request carrying the signed-in admin's cookie, or none at all.
    async fn call(
        state: &AppState,
        method: &str,
        body: Option<Value>,
        signed_in: bool,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri("/api/v1/company/chat/read-state")
            .header("content-type", "application/json");
        if signed_in {
            request = request.header("cookie", crate::server::test_support::fixed_cookie("acme"));
        }
        let request = match body {
            Some(value) => request.body(Body::from(value.to_string())).unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// The happy path, so the failure cases below are not the only coverage:
    /// a marker round-trips, and the write answers with where it stands.
    #[tokio::test]
    async fn a_marker_round_trips_for_a_signed_in_person() {
        let home = home();
        let state = state(home.path()).await;

        let (status, listed) = call(&state, "GET", None, true).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["markers"].as_array().unwrap().len(), 0);

        let (status, marked) = call(
            &state,
            "PUT",
            Some(json!({"channelId": "engineering", "lastReadAt": 2_000})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(marked["lastReadAt"], 2_000);

        // Monotonic at the HTTP boundary too, not only in the store: the reply
        // is the stored marker, not the number that was sent.
        let (_, back) = call(
            &state,
            "PUT",
            Some(json!({"channelId": "engineering", "lastReadAt": 500})),
            true,
        )
        .await;
        assert_eq!(back["lastReadAt"], 2_000);
    }

    /// A **machine** credential — authenticated, owns the company, but names no
    /// person — must not read or write markers. Otherwise one tenant's
    /// automation could clear a real operator's badges.
    ///
    /// The token is the point. An *unauthenticated* request also answers 401,
    /// but from the auth layer above this module, so asserting that would pass
    /// with this handler's own guard deleted. This drives the platform scope so
    /// the request reaches the handler with `actor: None`.
    #[tokio::test]
    async fn a_machine_credential_naming_no_person_is_refused_on_both_verbs() {
        use crate::server::platform_auth::{
            PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier,
        };
        use std::collections::HashSet;

        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let verifier = std::sync::Arc::new(UnsignedTenantVerifier::new("plat-secret"));
        let manifest: CompanyManifest = toml::from_str(MANIFEST).unwrap();
        let id = CompanyId::new("acme");
        let store = FsCompanyStore::new(home.clone());
        store
            .save(&CompanyRecord {
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
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
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.clone(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default())
            .with_home(home.clone())
            .with_platform_auth(PlatformAuthConfig::new(verifier));
        state
            .registry()
            .insert(id.clone(), std::sync::Arc::new(runtime));
        state.set_owner(id.clone(), "tenant:acme".to_string());

        let token = UnsignedTenantVerifier::tenant_token(&PlatformClaims {
            tenant: "tenant:acme".to_string(),
            scopes: HashSet::from(["operator".to_string()]),
            companies: None,
        });

        let send = async |method: &str, body: Option<Value>| {
            let mut request = Request::builder()
                .method(method)
                .uri("/api/v1/companies/acme/chat/read-state")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json");
            let _ = &mut request;
            let request = match body {
                Some(v) => request.body(Body::from(v.to_string())).unwrap(),
                None => request.body(Body::empty()).unwrap(),
            };
            let response = router(state.clone()).oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        };

        let (status, body) = send("GET", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "unauthorized");

        let (status, _) = send(
            "PUT",
            Some(json!({"channelId": "engineering", "lastReadAt": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// A blank channel id is refused rather than stored — a marker on `""`
    /// would be unreachable from the console and invisible in the list.
    #[tokio::test]
    async fn a_blank_channel_id_is_refused() {
        let home = home();
        let state = state(home.path()).await;

        for blank in ["", "   "] {
            let (status, body) = call(
                &state,
                "PUT",
                Some(json!({"channelId": blank, "lastReadAt": 1})),
                true,
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "blank {blank:?}");
            assert_eq!(body["code"], "invalid_request");
        }

        // Nothing was stored by the refused writes.
        let (_, listed) = call(&state, "GET", None, true).await;
        assert_eq!(listed["markers"].as_array().unwrap().len(), 0);
    }
}
