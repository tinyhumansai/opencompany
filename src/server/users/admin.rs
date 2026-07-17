//! Admin routes: the invite list, the roster, and password reset.
//!
//! Every route here requires a live session belonging to a user whose role is
//! [`UserRole::Admin`]. There is no operator break-glass, because the operator
//! token is dead configuration (see `docs/spec/runtime/config.md`); the
//! manifest's `[users] admins` list is what bootstraps the first admin, and
//! these routes are how admins manage everyone after that.
//!
//! ## The last-admin rule
//!
//! Demoting, suspending, or deleting the final active admin is refused. Without
//! that, a company can lock itself out of its own user directory in one click
//! and there is nothing to recover with — no operator token, and the manifest
//! only bootstraps addresses it names. Editing the manifest would be the only
//! way back, which a hosted tenant may not be able to do.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::{
    InviteRecord, UserRecord, UserRole, UserStatus, generate_id, normalize_email, now_millis,
};
use crate::server::error::ApiError;
use crate::server::graphql::auth::UserPrincipal;
use crate::server::users::routes::{current_user, manifest_admin_invites};
use crate::server::users::scope::{PublicCompany, public_scoped};
use crate::server::users::{password, token};

/// How long an admin-sent invite stays redeemable.
const INVITE_TTL_MILLIS: u64 = 14 * 24 * 60 * 60 * 1000;

/// Builds the admin route fragment.
pub fn router() -> Router<AppState> {
    public_scoped("/users", get(list_users))
        .merge(public_scoped(
            "/users/invites",
            get(list_invites).post(invite),
        ))
        .merge(public_scoped(
            "/users/invites/{invite_id}",
            delete(revoke_invite),
        ))
        .merge(public_scoped("/users/{user_id}", patch(update_user)))
        .merge(public_scoped(
            "/users/{user_id}/password",
            post(reset_password),
        ))
        .merge(public_scoped(
            "/users/{user_id}/sessions",
            delete(revoke_sessions),
        ))
}

/// `403` for an authenticated non-admin.
fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "only an admin can do that",
            "code": "forbidden",
        })),
    )
        .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "not signed in", "code": "unauthorized" })),
    )
        .into_response()
}

/// Requires a live session whose user is an admin of this company.
async fn require_admin(
    headers: &HeaderMap,
    state: &AppState,
    runtime: &CompanyRuntime,
) -> Result<UserPrincipal, Response> {
    let principal = current_user(headers, state, runtime.id())
        .await
        .ok_or_else(unauthorized)?;
    if !principal.may_administer() {
        return Err(forbidden());
    }
    Ok(principal)
}

/// A user as an admin sees them. Never carries the password hash.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserSummary {
    id: String,
    email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    role: UserRole,
    status: UserStatus,
    /// Whether they have a password, never what it is.
    has_password: bool,
    must_change_password: bool,
    created_at_millis: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen_at_millis: Option<u64>,
}

impl From<UserRecord> for UserSummary {
    fn from(u: UserRecord) -> Self {
        Self {
            id: u.id,
            email: u.email,
            display_name: u.display_name,
            role: u.role,
            status: u.status,
            has_password: u.password_hash.is_some(),
            must_change_password: u.must_change_password,
            created_at_millis: u.created_at_millis,
            last_seen_at_millis: u.last_seen_at_millis,
        }
    }
}

/// `GET …/users` — the roster.
async fn list_users(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<UserSummary>>, Response> {
    let runtime = company.runtime.clone();
    require_admin(&headers, &state, &runtime).await?;
    let users = runtime
        .users()
        .list_users(runtime.id())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    Ok(Json(users.into_iter().map(UserSummary::from).collect()))
}

/// `GET …/users/invites` — outstanding invites, including manifest admins.
async fn list_invites(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<InviteRecord>>, Response> {
    let runtime = company.runtime.clone();
    require_admin(&headers, &state, &runtime).await?;
    let now = now_millis();
    let mut invites = runtime
        .users()
        .list_invites(runtime.id())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    // Manifest admins are eligible without an invite record. Showing only the
    // stored ones would render a list that contradicts who can actually log in.
    let stored: Vec<String> = invites.iter().map(|i| i.email.clone()).collect();
    let users = runtime
        .users()
        .list_users(runtime.id())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let synthetic = manifest_admin_invites(&runtime, now)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    for invite in synthetic {
        let already_a_user = users.iter().any(|u| u.email == invite.email);
        if !stored.contains(&invite.email) && !already_a_user {
            invites.push(invite);
        }
    }
    Ok(Json(invites))
}

#[derive(Debug, Deserialize)]
struct InviteBody {
    email: String,
    #[serde(default)]
    role: UserRole,
}

/// `POST …/users/invites` — invite an address.
async fn invite(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<InviteBody>,
) -> Result<Json<InviteRecord>, Response> {
    let runtime = company.runtime.clone();
    let admin = require_admin(&headers, &state, &runtime).await?;
    let email = normalize_email(&body.email);
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "that doesn't look like an email address".to_string(),
        ))
        .into_response());
    }
    let now = now_millis();
    if runtime
        .users()
        .find_user_by_email(runtime.id(), &email)
        .await
        .map_err(|e| ApiError(e).into_response())?
        .is_some()
    {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "{email} is already a member"
        )))
        .into_response());
    }
    let record = InviteRecord {
        id: generate_id(),
        email,
        role: body.role,
        invited_by: admin.user_id.clone(),
        created_at_millis: now,
        expires_at_millis: now + INVITE_TTL_MILLIS,
        accepted_at_millis: None,
    };
    // The store enforces one invite per address; a clash surfaces as 409.
    runtime
        .users()
        .upsert_invite(runtime.id(), &record)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    Ok(Json(record))
}

/// `DELETE …/users/invites/{invite_id}` — revoke an invite.
async fn revoke_invite(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Response> {
    let runtime = company.runtime.clone();
    require_admin(&headers, &state, &runtime).await?;
    let invite_id = params.get("invite_id").cloned().unwrap_or_default();
    // A manifest admin has no stored invite; revoking it would be a lie, since
    // the manifest would re-grant on the next login.
    if invite_id.starts_with("manifest:") {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "this admin comes from the company manifest; remove them from \
             [users].admins there instead"
                .to_string(),
        ))
        .into_response());
    }
    let removed = runtime
        .users()
        .delete_invite(runtime.id(), &invite_id)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    Ok(Json(serde_json::json!({ "removed": removed })))
}

#[derive(Debug, Deserialize)]
struct UpdateUser {
    #[serde(default)]
    role: Option<UserRole>,
    #[serde(default)]
    status: Option<UserStatus>,
    #[serde(default)]
    display_name: Option<String>,
}

/// `PATCH …/users/{user_id}` — change a role, suspend, or reactivate.
async fn update_user(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<UpdateUser>,
) -> Result<Json<UserSummary>, Response> {
    let runtime = company.runtime.clone();
    require_admin(&headers, &state, &runtime).await?;
    let user_id = params.get("user_id").cloned().unwrap_or_default();
    let mut user = load_user(&runtime, &user_id).await?;

    let losing_admin = matches!(body.role, Some(UserRole::Member))
        || matches!(body.status, Some(UserStatus::Suspended));
    if losing_admin {
        ensure_not_last_admin(&runtime, &user).await?;
    }
    if let Some(role) = body.role {
        user.role = role;
    }
    if let Some(status) = body.status {
        user.status = status;
    }
    if let Some(name) = body.display_name {
        user.display_name = Some(name);
    }
    user.updated_at_millis = now_millis();
    runtime
        .users()
        .upsert_user(runtime.id(), &user)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    // Suspension must bite now, not at cookie expiry. resolve_principal also
    // re-checks status per request; this closes the window and frees the rows.
    if user.status == UserStatus::Suspended {
        let _ = runtime
            .sessions()
            .delete_for_user(runtime.id(), &user.id)
            .await;
        let _ = runtime
            .login_codes()
            .delete_for_email(runtime.id(), &user.email)
            .await;
    }
    Ok(Json(user.into()))
}

#[derive(Debug, Deserialize)]
struct ResetPassword {
    /// The temporary password to set. The admin conveys it out-of-band.
    password: String,
}

/// `POST …/users/{user_id}/password` — set a temporary password.
///
/// The admin chooses the value and tells the user through some other channel.
/// This unavoidably means an admin knows a user's password, which is why the
/// account is flagged [`must_change_password`](crate::ports::UserRecord) and
/// every existing session is revoked.
async fn reset_password(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<ResetPassword>,
) -> Result<Json<UserSummary>, Response> {
    let runtime = company.runtime.clone();
    require_admin(&headers, &state, &runtime).await?;
    let user_id = params.get("user_id").cloned().unwrap_or_default();
    let mut user = load_user(&runtime, &user_id).await?;

    password::validate(&body.password, &user.email).map_err(|e| ApiError(e).into_response())?;
    let hash = password::hash(&token::OsTokens, &body.password)
        .map_err(|e| ApiError(e).into_response())?;
    user.password_hash = Some(hash);
    user.must_change_password = true;
    user.updated_at_millis = now_millis();
    runtime
        .users()
        .upsert_user(runtime.id(), &user)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    // Every session goes: a reset is what you do when you believe the account
    // is compromised, so leaving live sessions running would defeat it.
    let _ = runtime
        .sessions()
        .delete_for_user(runtime.id(), &user.id)
        .await;
    let _ = runtime
        .login_codes()
        .delete_for_email(runtime.id(), &user.email)
        .await;
    Ok(Json(user.into()))
}

/// `DELETE …/users/{user_id}/sessions` — sign a user out everywhere.
async fn revoke_sessions(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Response> {
    let runtime = company.runtime.clone();
    require_admin(&headers, &state, &runtime).await?;
    let user_id = params.get("user_id").cloned().unwrap_or_default();
    let user = load_user(&runtime, &user_id).await?;
    let revoked = runtime
        .sessions()
        .delete_for_user(runtime.id(), &user.id)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    Ok(Json(serde_json::json!({ "revoked": revoked })))
}

async fn load_user(runtime: &CompanyRuntime, user_id: &str) -> Result<UserRecord, Response> {
    runtime
        .users()
        .get_user(runtime.id(), user_id)
        .await
        .map_err(|e| ApiError(e).into_response())?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::InvalidRequest(format!(
                "no user {user_id}"
            )))
            .into_response()
        })
}

/// Refuses to strip admin from the last active admin.
async fn ensure_not_last_admin(
    runtime: &CompanyRuntime,
    target: &UserRecord,
) -> Result<(), Response> {
    if target.role != UserRole::Admin || target.status != UserStatus::Active {
        return Ok(());
    }
    let users = runtime
        .users()
        .list_users(runtime.id())
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let others = users
        .iter()
        .filter(|u| {
            u.id != target.id && u.role == UserRole::Admin && u.status == UserStatus::Active
        })
        .count();
    if others == 0 {
        return Err(ApiError(OpenCompanyError::Conflict(
            "this is the company's last admin; promote someone else first".to_string(),
        ))
        .into_response());
    }
    Ok(())
}
