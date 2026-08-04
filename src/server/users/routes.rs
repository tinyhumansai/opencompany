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
//!
//! A platform-provisioned company has an empty list — its creator is recorded
//! on the control plane, not in the manifest — so the deployment may name one
//! more standing admin through [`AppConfig::bootstrap_admin`]. It is the same
//! kind of grant, not a second one: eligibility only, minted on redemption,
//! revoked by unsetting the source.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

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
use crate::{AppConfig, AppState};

/// How long a manifest-bootstrapped admin invite stays redeemable once
/// materialized. Long, because it is regenerated from the manifest on demand.
const MANIFEST_INVITE_TTL_MILLIS: u64 = 30 * 24 * 60 * 60 * 1000;

/// The soonest a second link may be mailed to one address.
///
/// Without this, `auth/request` is a mail cannon anyone can aim at an invited
/// mailbox. Long enough to stop that; short enough that a genuine "it didn't
/// arrive, send another" is not an ordeal.
///
/// Hitting the throttle returns the **same** `202` as sending, and does not
/// disturb the live code — otherwise the throttle would itself be the
/// membership oracle the rest of this module refuses to be, and an attacker
/// could invalidate a victim's link on demand.
///
/// It applies only where a mail can actually go out; see
/// [`echoes_code_in_response`].
const RESEND_INTERVAL_MILLIS: u64 = 60 * 1000;

/// Builds the user-auth route fragment.
pub fn router() -> Router<AppState> {
    public_scoped("/auth/request", post(request_code))
        .merge(public_scoped("/auth/verify", post(verify_code)))
        .merge(public_scoped("/auth/login", post(login_password)))
        .merge(public_scoped("/auth/logout", post(logout)))
        .merge(public_scoped("/auth/me", get(me)))
        .merge(public_scoped(
            "/auth/hub",
            get(hub_providers).post(hub_sign_in),
        ))
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

/// A platform token, handed back by the hub on the sign-in redirect.
#[derive(Debug, Deserialize)]
struct HubToken {
    token: String,
}

/// One sign-in button, ready to render.
///
/// The console never assembles a hub URL itself. Only the host knows the hub's
/// base URL and the origin the hub must return to, and a frontend guessing at
/// either would aim a live sign-in at whatever it guessed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HubProviderOption {
    id: &'static str,
    label: &'static str,
    start_url: String,
}

/// What the console needs to draw its sign-in screen.
#[derive(Debug, Serialize)]
struct HubProvidersResult {
    /// Empty on every host with no hub wired, which is how the console knows to
    /// render the magic-link form alone rather than buttons that lead nowhere.
    providers: Vec<HubProviderOption>,
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

/// The addresses this company bootstraps as admins without an invite record:
/// the manifest's `[users] admins`, plus the deployment's
/// [`AppConfig::bootstrap_admin`] when one is injected.
///
/// Both are the same grant, so they are one list — deduplicated, because an
/// address named in both places is still one standing invite and must render as
/// one row on the invite page.
pub(crate) async fn bootstrap_admins(
    config: &AppConfig,
    runtime: &CompanyRuntime,
) -> Result<Vec<String>, OpenCompanyError> {
    Ok(with_platform_admin(config, manifest_admins(runtime).await?))
}

/// Appends the deployment's bootstrap admin to a manifest admin list.
///
/// Split out so the invite listing can tell the two sources apart without
/// reading the manifest twice.
fn with_platform_admin(config: &AppConfig, mut admins: Vec<String>) -> Vec<String> {
    if let Some(email) = config.bootstrap_admin()
        && !admins.contains(&email)
    {
        admins.push(email);
    }
    admins
}

/// Whether `email` may hold an account in this company, and as what role.
///
/// Three ways in, checked in order:
/// 1. They already are a user (their role stands).
/// 2. A [`bootstrap_admins`] entry names them — the bootstrap path.
/// 3. An admin invited them, and the invite is still redeemable.
///
/// `None` means the address gets no code and no session, indistinguishably.
async fn eligibility(
    config: &AppConfig,
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
    if bootstrap_admins(config, runtime)
        .await?
        .iter()
        .any(|a| a == email)
    {
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

    // Record the sign-in on the user. Every session is minted here — link and
    // password alike — so this is the one place that makes `last_seen` mean
    // "last signed in" rather than "joined". It is deliberately not updated per
    // request: that would be a store write on every authenticated call, and
    // knowing someone signed in an hour ago is not worth that.
    let mut signed_in = user.clone();
    signed_in.last_seen_at_millis = Some(now);
    signed_in.updated_at_millis = now;
    let _ = runtime.users().upsert_user(company, &signed_in).await;

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

    let eligible = eligibility(state.config(), &runtime, &email, now)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let Some(_role) = eligible else {
        // Unknown or uninvited: no code, no mail, same answer.
        return Ok(Json(RequestCodeResult {
            sent: true,
            dev_code: None,
        }));
    };

    // Throttle. Checked after eligibility so an ineligible address never
    // reaches a store read that an eligible one does — and answered with the
    // same 202, so the throttle is not itself an oracle. The live code is left
    // alone: replacing it here would let anyone kill a victim's link at will.
    //
    // Skipped entirely where nothing is mailed and the code comes back in the
    // response instead (issue #271). Only the plaintext's *hash* is stored, so
    // a throttled response cannot re-echo the live code — it hands the caller
    // an acknowledgement and no way in, for a minute after every single sign-in.
    // On a loopback host with no transport that is not rate-limiting a mail
    // cannon, it is the only sign-in path locking itself.
    if !echoes_code_in_response(&state)
        && let Some(previous) = runtime
            .login_codes()
            .latest_for_email(runtime.id(), &email)
            .await
            .map_err(|e| ApiError(e).into_response())?
        && now.saturating_sub(previous.created_at_millis) < RESEND_INTERVAL_MILLIS
    {
        tracing::debug!(company = %runtime.id(), "login link throttled");
        return Ok(Json(RequestCodeResult {
            sent: true,
            dev_code: None,
        }));
    }

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

/// Whether this host has a mail transport at all.
///
/// Not "will this send succeed" — a wired transport that errors still counts,
/// because the attempt is what the resend throttle rate-limits.
fn mail_transport_wired(state: &AppState) -> bool {
    let connections = state.connections();
    connections.mail.is_some() && connections.mail_credentials.is_some()
}

/// Whether a minted code comes back in the response instead of going to a
/// mailbox.
///
/// Exactly the shape the dev echo is already gated on: a loopback bind, no
/// `public_url`, and no mail transport wired. Nothing leaves the machine in
/// that shape — there is no mailbox to flood and no remote caller to leak to —
/// which is what makes skipping the resend throttle there safe. Any other host
/// keeps the throttle.
fn echoes_code_in_response(state: &AppState) -> bool {
    state.config().is_local_only() && !mail_transport_wired(state)
}

/// Mails the magic link. Returns whether it was actually sent.
async fn deliver_code(state: &AppState, runtime: &CompanyRuntime, email: &str, code: &str) -> bool {
    // Asked through the shared predicate so "can this host mail at all" has one
    // answer: the throttle and the dev echo both branch on it, and a second
    // spelling here is how those three drift apart.
    if !mail_transport_wired(state) {
        return false;
    }
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
    let Some(role) = eligibility(state.config(), &runtime, &code.email, now)
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

/// `403` for an ecosystem sign-in this host cannot or will not honor.
///
/// Unlike [`invalid_login`], these say what went wrong. The generic-failure
/// rule exists so the login routes cannot be used as a membership oracle, and
/// nothing here leaks membership: "this host has no hub" is a fact about the
/// *deployment* the caller already knew, and "the hub rejected that" is about
/// the token. `not_a_member` is the one that touches a person, and it is only
/// ever reached by someone who has just proved to the hub that they hold that
/// address — they are not learning anything they did not already know.
fn hub_refused(code: &'static str, message: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": message, "code": code })),
    )
        .into_response()
}

/// Where the hub sends the browser back to after a sign-in.
///
/// Built from [`AppConfig::host_base_url`](crate::AppConfig::host_base_url) —
/// the configured `OPENCOMPANY_PUBLIC_URL` when there is one, otherwise
/// `http://{bind}`. That single seam is what makes hosted a configuration
/// change rather than a code change: locally the bind fallback yields
/// `http://127.0.0.1:<port>/`, which is the RFC 8252 loopback URI the hub
/// already accepts; hosted, `OPENCOMPANY_PUBLIC_URL` yields the real origin and
/// this function is untouched.
///
/// Carries `?company=` so the console lands scoped to the company it left from.
/// The hub appends its own `token=…&key=auth` with `&`, so the two coexist.
fn console_redirect_uri(state: &AppState, company: &CompanyId) -> String {
    let origin = state.config().host_base_url();
    format!("{}/?company={}", origin.trim_end_matches('/'), company)
}

/// `GET …/auth/hub` — the ecosystem sign-in buttons, ready to render.
///
/// Answers `{"providers": []}` rather than a 404 on a host with no hub, so the
/// console has one code path: ask, render what comes back, and fall through to
/// the magic-link form when nothing does.
async fn hub_providers(
    company: PublicCompany,
    State(state): State<AppState>,
) -> Json<HubProvidersResult> {
    // No exchange means no way to check a token that came back, so there is no
    // honest button to offer. Refusing here — rather than at redemption — is
    // the difference between a console that says "sign in with a link" and one
    // that sends someone through Google to be turned away on return.
    if state.hub_identity().is_none() {
        return Json(HubProvidersResult {
            providers: Vec::new(),
        });
    }
    let redirect_uri = console_redirect_uri(&state, company.runtime.id());
    let api_url = &state.config().api_url;
    Json(HubProvidersResult {
        providers: crate::server::hub_identity::HUB_PROVIDERS
            .iter()
            .map(|provider| HubProviderOption {
                id: provider.id,
                label: provider.label,
                start_url: crate::server::hub_identity::login_start_url(
                    api_url,
                    provider.id,
                    &redirect_uri,
                ),
            })
            .collect(),
    })
}

/// `POST …/auth/hub` — turn an ecosystem sign-in into a session here.
///
/// The console sends the browser to the hub's OAuth start pointed back at this
/// origin; the hub completes the provider dance and returns a platform JWT in
/// the URL. This route takes that token, asks the hub whose it is, and — if
/// that address is eligible in *this* company by the same rules a magic link
/// answers to — mints an ordinary human session.
///
/// It is deliberately the same three calls the magic-link path makes:
/// [`eligibility`], [`upsert_from_eligibility`], [`mint_session`]. First login
/// and Nth login stay one code path, and an ecosystem sign-in gets no privilege
/// a mailed link would not have given the same person. In particular this does
/// not touch `platform_auth`: that surface is the hosting layer's machine
/// credential, and a human signing in must not acquire one.
///
/// The token is used for exactly one outbound request and then dropped. It is
/// never persisted, never logged, and never echoed into an error.
async fn hub_sign_in(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<HubToken>,
) -> Result<Response, Response> {
    let runtime = company.runtime.clone();

    // Refuse before going anywhere when this host has no hub to ask. Accepting
    // the token on trust would make an unverifiable JWT a bearer credential.
    let Some(exchange) = state.hub_identity().cloned() else {
        return Err(hub_refused(
            "hub_unavailable",
            "this host is not part of a TinyHumans ecosystem",
        ));
    };

    // The hub answering is what proves the token was real — this tenant cannot
    // check the signature and does not try. Everything below reasons about the
    // identity the hub returned, never about the request body.
    let identity = match exchange.identify(&body.token).await {
        Ok(identity) => identity,
        // A 4xx from the hub means the token is expired, revoked, or was never
        // real — a dead credential, not a broken hub. Surfacing the 502 the
        // error type otherwise maps to would tell the user the ecosystem is
        // down when all they need to do is sign in again.
        Err(OpenCompanyError::TinyHumans { code, .. }) if code.starts_with("http_4") => {
            return Err(hub_refused(
                "hub_rejected",
                "that sign-in has expired — sign in again",
            ));
        }
        // Anything else really is the hub being unreachable or wrong, and keeps
        // its 502/503 so an operator can tell the two apart.
        Err(err) => return Err(ApiError(err).into_response()),
    };

    let email = normalize_email(&identity.email);
    let now = now_millis();
    let Some(role) = eligibility(state.config(), &runtime, &email, now)
        .await
        .map_err(|e| ApiError(e).into_response())?
    else {
        // Signed in to the ecosystem, but not a person this company knows. A
        // distinct code so the console can say "ask an admin to invite you"
        // instead of "that sign-in is dead".
        return Err(hub_refused(
            "not_a_member",
            "that account has no access to this company",
        ));
    };
    let user = upsert_from_eligibility(&runtime, &email, role, now)
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

/// Materializes [`bootstrap_admins`] as invite records.
///
/// Exposed for the admin routes, so listing invites shows the bootstrapped
/// admins rather than an empty page that contradicts who can actually log in.
/// These are synthetic — no such row exists — which is why their ids are
/// prefixed `manifest:` / `platform:` and revoking one is refused.
///
/// The prefix names the source because the two are withdrawn in different
/// places: a `manifest:` row goes away by editing `[users].admins`, a
/// `platform:` row by unsetting the deployment's variable. An address in both
/// renders as `manifest:` — that is the grant that outlives the variable.
pub(crate) async fn manifest_admin_invites(
    config: &AppConfig,
    runtime: &CompanyRuntime,
    now: u64,
) -> Result<Vec<InviteRecord>, OpenCompanyError> {
    let from_manifest = manifest_admins(runtime).await?;
    Ok(with_platform_admin(config, from_manifest.clone())
        .into_iter()
        .map(|email| {
            let source = if from_manifest.contains(&email) {
                "manifest"
            } else {
                "platform"
            };
            InviteRecord {
                id: format!("{source}:{email}"),
                email,
                role: UserRole::Admin,
                invited_by: source.to_string(),
                created_at_millis: now,
                expires_at_millis: now + MANIFEST_INVITE_TTL_MILLIS,
                accepted_at_millis: None,
            }
        })
        .collect())
}
