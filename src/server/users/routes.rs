//! The login routes: magic link, password, session, logout.
//!
//! ## The generic-failure rule
//!
//! `POST …/auth/request` **always** returns `202 {"sent": true}`, and
//! `POST …/auth/verify` and `…/auth/login` **always** fail with one identical
//! `401 invalid_login`. Not for tidiness: any difference between "no such
//! address here" and "wrong secret" turns these routes into a membership
//! oracle for the company. Someone who can ask "is bob@acme.com a user of this
//! company?" learns the org chart, and every answer is a phishing target.
//!
//! That rule is why the failure paths look repetitive and why
//! [`password::dummy_verify`] is called where there is nothing to verify —
//! response *time* would otherwise answer what the response body refuses to.
//!
//! ## Bootstrap
//!
//! Access is invite-only, so someone must send the first invite. There is no
//! operator token to do it with, so the company manifest's `[users] admins`
//! list is the root of trust: those addresses are standing admin invites.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::ports::{
    InviteRecord, LoginCodeRecord, SessionRecord, UserRecord, UserRole, UserStatus, generate_id,
    normalize_email, now_millis,
};
use crate::server::error::ApiError;
use crate::server::graphql::auth::{GqlAuth, UserPrincipal, resolve_principal};
use crate::server::ops::mailer::OutboundEmail;
use crate::server::users::scope::{PublicCompany, public_scoped};
use crate::server::users::{cookie, password, token};

/// How long a manifest-bootstrapped admin invite stays redeemable once
/// materialized. Long, because it is regenerated from the manifest on demand.
const MANIFEST_INVITE_TTL_MILLIS: u64 = 30 * 24 * 60 * 60 * 1000;

// KNOWN GAP: there is no resend throttle. Repeated `auth/request` calls for one
// invited address will mail a link each time, so the route can be pointed at an
// invited mailbox as a nuisance. It is not an account-takeover path (each link
// invalidates the last, and only the mailbox owner can read them) and it needs
// an invited address to aim at, so it is recorded rather than fixed here:
// throttling needs a lookup-by-email on LoginCodeStore, which is a port change
// across three backends and belongs in its own slice.

/// Builds the user-auth route fragment.
pub fn router() -> Router<AppState> {
    public_scoped("/auth/request", post(request_code))
        .merge(public_scoped("/auth/verify", post(verify_code)))
        .merge(public_scoped("/auth/login", post(login_password)))
        .merge(public_scoped("/auth/logout", post(logout)))
        .merge(public_scoped("/auth/me", get(me)))
        .merge(public_scoped("/auth/password", post(set_password)))
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RequestCode {
    email: String,
}

#[derive(Debug, Serialize)]
struct RequestCodeResult {
    /// Always `true`. Whether a mail was actually sent is deliberately not
    /// reported — that would be the oracle this route exists to avoid.
    sent: bool,
    /// The login code, echoed **only** when the host binds loopback *and* has
    /// no mail transport. Absent on any host reachable from elsewhere, even
    /// when its mail is broken — a credential must never leave in a response
    /// to whoever asked for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    dev_code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VerifyCode {
    code: String,
}

#[derive(Debug, Deserialize)]
struct LoginPassword {
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct SetPassword {
    password: String,
}

/// The authenticated user, as the console sees them. Carries no secret.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeResult {
    id: String,
    email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    role: UserRole,
    company: String,
    /// Whether this user has a password set (vs magic-link only).
    has_password: bool,
    /// Whether an admin issued a temporary password that should be replaced.
    must_change_password: bool,
}

// ---------------------------------------------------------------------------
// Shared failures
// ---------------------------------------------------------------------------

/// The single failure every login path returns.
///
/// One message for: unknown address, uninvited address, no code issued,
/// expired code, already-used code, wrong code, wrong password, no password
/// set, suspended user. Distinguishing any of them leaks membership.
fn invalid_login() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "that didn't work — request a new login link",
            "code": "invalid_login",
        })),
    )
        .into_response()
}

/// `401` for a request with no live session where one is required.
fn no_session() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "not signed in", "code": "unauthorized" })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Eligibility
// ---------------------------------------------------------------------------

/// The company's manifest, read from its record.
///
/// The manifest is not cached on the runtime — it lives in the `CompanyStore`
/// — so this is a store read. It is on the login path only, which is cold.
pub(crate) async fn load_manifest(
    runtime: &CompanyRuntime,
) -> Result<Option<crate::company::CompanyManifest>, OpenCompanyError> {
    Ok(runtime
        .store()
        .load(runtime.id())
        .await?
        .map(|record| record.manifest))
}

/// The normalized addresses the manifest bootstraps as admins.
pub(crate) async fn manifest_admins(
    runtime: &CompanyRuntime,
) -> Result<Vec<String>, OpenCompanyError> {
    Ok(load_manifest(runtime)
        .await?
        .map(|m| m.users.admins.iter().map(|a| normalize_email(a)).collect())
        .unwrap_or_default())
}

/// Whether `email` may hold an account in this company, and as what role.
///
/// Three ways in, checked in order:
/// 1. They already are a user (their role stands).
/// 2. The manifest's `[users] admins` names them — the bootstrap path.
/// 3. An admin invited them, and the invite is still redeemable.
///
/// `None` means the address gets no code and no session, indistinguishably.
async fn eligibility(
    runtime: &CompanyRuntime,
    email: &str,
    now: u64,
) -> Result<Option<UserRole>, OpenCompanyError> {
    let id = runtime.id();
    if let Some(user) = runtime.users().find_user_by_email(id, email).await? {
        // A suspended user is not eligible — but says so with the same silence
        // as an unknown address.
        return Ok((user.status == UserStatus::Active).then_some(user.role));
    }
    if manifest_admins(runtime).await?.iter().any(|a| a == email) {
        return Ok(Some(UserRole::Admin));
    }
    let invite = runtime.users().find_invite_by_email(id, email).await?;
    Ok(invite.filter(|i| i.is_redeemable(now)).map(|i| i.role))
}

/// Returns the existing user for `email`, or materializes one from their
/// eligibility.
///
/// This is where an invite becomes an account. Redemption is not a separate
/// flow with its own credential: first login and Nth login are the same code
/// path, which is what keeps the two from drifting apart.
async fn upsert_from_eligibility(
    runtime: &CompanyRuntime,
    email: &str,
    role: UserRole,
    now: u64,
) -> Result<UserRecord, OpenCompanyError> {
    let id = runtime.id();
    if let Some(user) = runtime.users().find_user_by_email(id, email).await? {
        return Ok(user);
    }
    let user = UserRecord {
        id: generate_id(),
        email: email.to_string(),
        display_name: None,
        role,
        status: UserStatus::Active,
        password_hash: None,
        must_change_password: false,
        created_at_millis: now,
        last_seen_at_millis: Some(now),
        updated_at_millis: now,
    };
    runtime.users().upsert_user(id, &user).await?;
    // Mark any real invite as redeemed. A manifest-bootstrapped admin has no
    // invite record, so this is a no-op for them.
    if let Some(mut invite) = runtime.users().find_invite_by_email(id, email).await? {
        invite.accepted_at_millis = Some(now);
        runtime.users().upsert_invite(id, &invite).await?;
    }
    Ok(user)
}

// ---------------------------------------------------------------------------
// Session minting
// ---------------------------------------------------------------------------

/// Mints a session for `user` and renders the `Set-Cookie` response.
async fn mint_session(
    state: &AppState,
    runtime: &CompanyRuntime,
    user: &UserRecord,
    headers: &HeaderMap,
) -> Result<Response, Response> {
    let company = runtime.id();
    // A company whose id cannot safely name a cookie cannot hold a session;
    // refuse rather than emit a header its id could have chosen attributes for.
    let Some(name) = cookie::session_cookie_name(company) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "this company's id cannot carry a session cookie".to_string(),
        ))
        .into_response());
    };
    let now = now_millis();
    let plaintext = token::mint_session_token(&token::OsTokens);
    let session = SessionRecord {
        id: generate_id(),
        // Only the hash is persisted; the plaintext leaves in the cookie below
        // and is never written down.
        token_hash: token::sha256_hex(&plaintext),
        user_id: user.id.clone(),
        created_at_millis: now,
        expires_at_millis: now + token::SESSION_TTL_MILLIS,
        last_seen_at_millis: now,
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.chars().take(200).collect()),
    };
    runtime
        .sessions()
        .create(company, &session)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    // Opportunistic cleanup on a cold path, so no background task is needed.
    let _ = runtime.sessions().purge_expired(company, now).await;
    let _ = runtime.login_codes().purge_expired(company, now).await;

    let insecure = !state.config().host_base_url().starts_with("https://");
    let set = cookie::set_cookie(
        &name,
        &plaintext,
        token::SESSION_TTL_MILLIS / 1000,
        insecure,
    );
    let body = Json(me_result(runtime.id(), user));
    Ok(([(header::SET_COOKIE, set)], body).into_response())
}

fn me_result(company: &CompanyId, user: &UserRecord) -> MeResult {
    MeResult {
        id: user.id.clone(),
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        role: user.role,
        company: company.as_ref().to_string(),
        has_password: user.password_hash.is_some(),
        must_change_password: user.must_change_password,
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST …/auth/request` — mail a magic link.
///
/// Always `202`. See the module docs.
async fn request_code(
    company: PublicCompany,
    State(state): State<AppState>,
    Json(body): Json<RequestCode>,
) -> Result<Json<RequestCodeResult>, Response> {
    let email = normalize_email(&body.email);
    let runtime = company.runtime.clone();
    let now = now_millis();

    let eligible = eligibility(&runtime, &email, now)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let Some(_role) = eligible else {
        // Unknown or uninvited: no code, no mail, same answer.
        return Ok(Json(RequestCodeResult {
            sent: true,
            dev_code: None,
        }));
    };

    let plaintext = token::mint_login_code(&token::OsTokens);
    let record = LoginCodeRecord {
        id: generate_id(),
        code_hash: token::sha256_hex(&plaintext),
        email: email.clone(),
        created_at_millis: now,
        expires_at_millis: now + token::LOGIN_CODE_TTL_MILLIS,
        consumed_at_millis: None,
    };
    // One live code per address: issuing a new one invalidates the last, so a
    // link a user abandoned cannot be used later.
    runtime
        .login_codes()
        .delete_for_email(runtime.id(), &email)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    runtime
        .login_codes()
        .create(runtime.id(), &record)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    // Deliver. A send failure must not change the response — it would report
    // that the address exists.
    let delivered = deliver_code(&state, &runtime, &email, &plaintext).await;

    // Echoing the code makes local development work with no mail server. It is
    // also, literally, returning a credential in an HTTP response — so it is
    // gated on the host being unreachable from anywhere else, not merely on
    // mail being unconfigured. A routable host with broken mail fails to log
    // people in; it does not hand the credential to whoever asked.
    let local_only = state.config().is_local_only();
    let dev_code = (!delivered && local_only).then(|| plaintext.clone());
    if !delivered {
        if local_only {
            tracing::warn!(
                company = %runtime.id(),
                "no mail transport configured: returning the login code in the response. \
                 This only happens on a loopback bind. Configure OPENCOMPANY_MAIL_* \
                 before exposing this host."
            );
        } else {
            tracing::error!(
                company = %runtime.id(),
                "no mail transport configured and this host is routable, so the login \
                 code cannot be delivered and will NOT be echoed. Nobody can sign in \
                 until OPENCOMPANY_MAIL_* is configured."
            );
        }
    }
    Ok(Json(RequestCodeResult {
        sent: true,
        dev_code,
    }))
}

/// Mails the magic link. Returns whether it was actually sent.
async fn deliver_code(state: &AppState, runtime: &CompanyRuntime, email: &str, code: &str) -> bool {
    let connections = state.connections();
    let (Some(sender), Some(creds)) = (&connections.mail, &connections.mail_credentials) else {
        return false;
    };
    let base = state.config().host_base_url();
    let link = format!("{base}/login?company={}&code={code}", runtime.id().as_ref());
    let company_name = load_manifest(runtime)
        .await
        .ok()
        .flatten()
        .map(|m| m.company.name)
        .unwrap_or_else(|| runtime.id().as_ref().to_string());
    let mail = OutboundEmail {
        to: email.to_string(),
        subject: format!("Sign in to {company_name}"),
        body: format!(
            "Open this link to sign in to {company_name}:\n\n{link}\n\n\
             It expires in {} minutes and can only be used once. If you didn't \
             ask for it, you can ignore this — nothing has changed.\n",
            token::LOGIN_CODE_TTL_MILLIS / 60_000
        ),
    };
    match sender.send(creds, &mail).await {
        Ok(()) => true,
        Err(err) => {
            // Logged, not returned: the caller must not learn the address exists.
            tracing::warn!(company = %runtime.id(), "login mail failed: {err}");
            false
        }
    }
}

/// `POST …/auth/verify` — redeem a magic link for a session.
async fn verify_code(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<VerifyCode>,
) -> Result<Response, Response> {
    let runtime = company.runtime.clone();
    let now = now_millis();
    // Single use is the store's guarantee, not a check here: `consume` matches
    // and marks atomically, so two requests racing on one link cannot both win.
    let consumed = runtime
        .login_codes()
        .consume(runtime.id(), &token::sha256_hex(&body.code), now)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let Some(code) = consumed else {
        return Err(invalid_login());
    };

    // The address comes from the *code*, never from the request: otherwise
    // anyone holding any valid link could name whoever they liked.
    let Some(role) = eligibility(&runtime, &code.email, now)
        .await
        .map_err(|e| ApiError(e).into_response())?
    else {
        // Eligibility can lapse between mailing and clicking.
        return Err(invalid_login());
    };
    let user = upsert_from_eligibility(&runtime, &code.email, role, now)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    mint_session(&state, &runtime, &user, &headers).await
}

/// `POST …/auth/login` — exchange an email and password for a session.
async fn login_password(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginPassword>,
) -> Result<Response, Response> {
    let runtime = company.runtime.clone();
    let email = normalize_email(&body.email);
    let now = now_millis();

    let user = runtime
        .users()
        .find_user_by_email(runtime.id(), &email)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    // Every path with no hash to check burns equivalent work first, so an
    // unknown address costs the same wall-clock as a wrong password.
    let Some(user) = user else {
        password::dummy_verify(&body.password);
        return Err(invalid_login());
    };
    if user.status != UserStatus::Active {
        password::dummy_verify(&body.password);
        return Err(invalid_login());
    }
    let Some(hash) = user.password_hash.as_deref() else {
        // Magic-link-only account.
        password::dummy_verify(&body.password);
        return Err(invalid_login());
    };
    if !password::verify(&body.password, hash) {
        return Err(invalid_login());
    }

    let mut user = user;
    user.last_seen_at_millis = Some(now);
    user.updated_at_millis = now;
    let _ = runtime.users().upsert_user(runtime.id(), &user).await;
    mint_session(&state, &runtime, &user, &headers).await
}

/// `POST …/auth/logout` — revoke this session.
async fn logout(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let runtime = company.runtime.clone();
    let insecure = !state.config().host_base_url().starts_with("https://");
    let Some(name) = cookie::session_cookie_name(runtime.id()) else {
        return Err(no_session());
    };

    // Revoke server-side when the cookie names a real session; clearing the
    // cookie alone would leave a working token in whatever else holds it.
    if let Some(user) = current_user(&headers, &state, runtime.id()).await
        && let Ok(Some(session)) = runtime
            .sessions()
            .find_by_token_hash(runtime.id(), &user.session_token_hash)
            .await
    {
        let _ = runtime.sessions().delete(runtime.id(), &session.id).await;
    }
    // Always clear the cookie, even when nothing matched: logging out must be
    // idempotent and must never fail.
    Ok((
        [(header::SET_COOKIE, cookie::clear_cookie(&name, insecure))],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response())
}

/// `GET …/auth/me` — who this session belongs to.
async fn me(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MeResult>, Response> {
    let runtime = company.runtime.clone();
    let Some(principal) = current_user(&headers, &state, runtime.id()).await else {
        return Err(no_session());
    };
    let user = runtime
        .users()
        .get_user(runtime.id(), &principal.user_id)
        .await
        .map_err(|e| ApiError(e).into_response())?
        .ok_or_else(no_session)?;
    Ok(Json(me_result(runtime.id(), &user)))
}

/// `POST …/auth/password` — set or replace this user's own password.
///
/// Requires a live session, which is what makes a separate reset credential
/// unnecessary: a user who forgot their password logs in with a magic link and
/// lands here.
async fn set_password(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetPassword>,
) -> Result<Json<MeResult>, Response> {
    let runtime = company.runtime.clone();
    let Some(principal) = current_user(&headers, &state, runtime.id()).await else {
        return Err(no_session());
    };
    let mut user = runtime
        .users()
        .get_user(runtime.id(), &principal.user_id)
        .await
        .map_err(|e| ApiError(e).into_response())?
        .ok_or_else(no_session)?;

    password::validate(&body.password, &user.email).map_err(|e| ApiError(e).into_response())?;
    let hash = password::hash(&token::OsTokens, &body.password)
        .map_err(|e| ApiError(e).into_response())?;
    let now = now_millis();
    user.password_hash = Some(hash);
    // Whatever prompted the change is now satisfied.
    user.must_change_password = false;
    user.updated_at_millis = now;
    runtime
        .users()
        .upsert_user(runtime.id(), &user)
        .await
        .map_err(|e| ApiError(e).into_response())?;

    // Every *other* session is revoked: changing a password is what someone
    // does when they think a session is stolen, so leaving the others live
    // would defeat the point. This one survives so the user is not logged out
    // of the tab they just used.
    if let Ok(sessions) = runtime
        .sessions()
        .list_for_user(runtime.id(), &user.id)
        .await
    {
        for session in sessions {
            if session.token_hash != principal.session_token_hash {
                let _ = runtime.sessions().delete(runtime.id(), &session.id).await;
            }
        }
    }
    Ok(Json(me_result(runtime.id(), &user)))
}

/// The user behind this request's session cookie, if any.
pub(crate) async fn current_user(
    headers: &HeaderMap,
    state: &AppState,
    company: &CompanyId,
) -> Option<UserPrincipal> {
    match resolve_principal(headers, state, Some(company)).await {
        Ok(GqlAuth::User(user)) => Some(user),
        _ => None,
    }
}

/// Materializes the manifest's `[users] admins` as invite records.
///
/// Exposed for the admin routes, so listing invites shows the bootstrapped
/// admins rather than an empty page that contradicts who can actually log in.
/// These are synthetic — no such row exists — which is why their ids are
/// prefixed `manifest:` and revoking one is refused.
pub(crate) async fn manifest_admin_invites(
    runtime: &CompanyRuntime,
    now: u64,
) -> Result<Vec<InviteRecord>, OpenCompanyError> {
    Ok(manifest_admins(runtime)
        .await?
        .into_iter()
        .map(|email| InviteRecord {
            id: format!("manifest:{email}"),
            email,
            role: UserRole::Admin,
            invited_by: "manifest".to_string(),
            created_at_millis: now,
            expires_at_millis: now + MANIFEST_INVITE_TTL_MILLIS,
            accepted_at_millis: None,
        })
        .collect())
}
