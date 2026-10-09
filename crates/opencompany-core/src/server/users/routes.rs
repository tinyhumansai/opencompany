//! The login routes: magic link, password, first-admin claim, session, logout.
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

use crate::app::config::AuthMode;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::ports::{
    InviteRecord, LoginCodeRecord, LoginIdentity, SessionKind, SessionRecord, UserRecord, UserRole,
    UserStatus, generate_id, normalize_email, normalize_wallet, now_millis,
};
use crate::server::error::ApiError;
use crate::server::graphql::auth::{GqlAuth, UserPrincipal, resolve_principal};
use crate::server::ops::mailer::OutboundEmail;
use crate::server::users::scope::{PublicCompany, public_scoped};
use crate::server::users::{cookie, password, token, wallet};
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
///
/// Every route is mounted in every mode, and the ones a mode does not serve
/// refuse with [`not_this_mode`] rather than being left off the router. A
/// mounted-and-refusing route answers "this company does not sign in that way";
/// an absent one answers `404 no such route`, which is indistinguishable from a
/// version skew or a typo and would leave a client guessing.
pub fn router() -> Router<AppState> {
    public_scoped("/auth/config", get(auth_config))
        .merge(public_scoped("/auth/request", post(request_code)))
        .merge(public_scoped("/auth/verify", post(verify_code)))
        .merge(public_scoped("/auth/login", post(login_password)))
        .merge(public_scoped("/auth/logout", post(logout)))
        .merge(public_scoped("/auth/me", get(me).patch(edit_me)))
        .merge(public_scoped("/auth/claim", post(claim_first_admin)))
        .merge(public_scoped("/auth/password", post(set_password)))
        .merge(public_scoped(
            "/auth/wallet/challenge",
            post(wallet_challenge),
        ))
        .merge(public_scoped("/auth/wallet/verify", post(wallet_verify)))
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RequestCode {
    email: String,
    /// Where the console should land after this magic-link sign-in, carried as
    /// a URL fragment (`#/company`). Only setup's hand-off asks for one today;
    /// a normal sign-in omits it and lands wherever it always did. The value is
    /// mailed inside the login link, so it is validated to a conservative
    /// fragment subset — see [`redirect_fragment`].
    #[serde(default)]
    redirect: Option<String>,
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

/// The first admin's chosen login and password, on a company nobody has
/// joined yet. See [`claim_first_admin`].
#[derive(Debug, Deserialize)]
struct ClaimFirstAdmin {
    email: String,
    password: String,
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
    /// The face this person chose (`docs/spec/runtime/avatars.md`), absent when
    /// they have not chosen one.
    ///
    /// Absent is a real answer and is why the key is skipped rather than
    /// defaulted: the console draws the mascot it hashes from `id` in that case,
    /// and a client that could not tell the two apart would have no way to offer
    /// "use the default face again".
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar: Option<String>,
    role: UserRole,
    company: String,
    /// Whether this user has a password set (vs magic-link only).
    has_password: bool,
    /// Whether an admin issued a temporary password that should be replaced.
    must_change_password: bool,
}

/// What a successful sign-in returns.
///
/// [`MeResult`] is flattened rather than nested so this stays byte-identical to
/// what every login route returned before the header carrier existed — a client
/// that never asks for one sees no `session` field and needs no change.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SignInResult {
    #[serde(flatten)]
    user: MeResult,
    /// The ready-made [`SESSION_HEADER`](cookie::SESSION_HEADER) value, present
    /// **only** when the client asked for the header carrier.
    ///
    /// Returned exactly once, like a device pairing's token: only its hash is
    /// stored, so a client that drops it has to sign in again.
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
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
pub(crate) fn no_session() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "not signed in", "code": "unauthorized" })),
    )
        .into_response()
}

/// `409` for a login route this company's [`AuthMode`] does not serve.
///
/// It names the mode, which does **not** breach the generic-failure rule above:
/// the rule protects who is on the roster, and the mode is a property of the
/// deployment that [`auth_config`] already publishes to anonymous callers —
/// because the console cannot draw a sign-in screen without knowing it.
///
/// `409` rather than `404` because the route exists and the request was
/// well-formed; what is wrong is the state of the company it was aimed at. A
/// `404` would be indistinguishable from a version skew and would send a client
/// looking for a spelling mistake.
pub(crate) fn not_this_mode(mode: AuthMode) -> Response {
    let message = match mode {
        AuthMode::Email => "this company signs in by email",
        AuthMode::Wallet => "this company signs in with a wallet",
        AuthMode::None => "this company has no sign-in — it is used from the app on this device",
    };
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": message,
            "code": "auth_mode",
            "mode": mode.as_str(),
        })),
    )
        .into_response()
}

/// The refusal to return when this company does not sign in over email, and
/// `None` when it does.
///
/// `Option`, not `Result<(), Response>`: an axum `Response` is 128+ bytes and
/// `clippy::result_large_err` is right that a guard whose success carries no
/// value should not make every passing call pay the refusal's footprint. Same
/// shape as the sibling guards in `ops::repos` and `ops::team`.
pub(crate) fn wrong_mode_for_email(runtime: &CompanyRuntime) -> Option<Response> {
    (!runtime.auth_mode().uses_email()).then(|| not_this_mode(runtime.auth_mode()))
}

/// The refusal to return when this company does not sign in with a wallet.
fn wrong_mode_for_wallet(runtime: &CompanyRuntime) -> Option<Response> {
    (runtime.auth_mode() != AuthMode::Wallet).then(|| not_this_mode(runtime.auth_mode()))
}

/// The refusal to return when this company has no sign-in at all.
pub(crate) fn wrong_mode_for_login(runtime: &CompanyRuntime) -> Option<Response> {
    (!runtime.auth_mode().has_login()).then(|| not_this_mode(runtime.auth_mode()))
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
fn with_platform_admin(config: &AppConfig, admins: Vec<String>) -> Vec<String> {
    // Delegated so the host-side `issue-password` command and this route cannot
    // come to different answers about who a company already admits (#1718).
    super::bootstrap::standing_admins(&admins, config.bootstrap_admin().as_deref())
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
    // Which bootstrap list is consulted follows the sign-in mode, because the
    // two name different things: `[users].admins` holds mailboxes and
    // `[users].wallets` holds keys, and checking a wallet key against the email
    // list would be checking it against addresses it can never equal. `none`
    // mode has no bootstrap list at all — its single local owner is not
    // eligibility, it is the absence of the question.
    let bootstrapped = match runtime.auth_mode() {
        AuthMode::Email => bootstrap_admins(config, runtime).await?,
        AuthMode::Wallet => wallet::manifest_wallets(runtime).await?,
        AuthMode::None => Vec::new(),
    };
    if bootstrapped.iter().any(|a| a == email) {
        return Ok(Some(UserRole::Admin));
    }
    let invite = runtime.users().find_invite_by_email(id, email).await?;
    Ok(invite.filter(|i| i.is_redeemable(now)).map(|i| i.role))
}

/// Write `user`, or adopt the record that reached its address first.
///
/// Every caller below is find-then-create: look the address up, and mint a
/// record when it is absent. That pair is not atomic, and each caller mints a
/// **fresh `generate_id()`**, so two requests arriving together both miss the
/// lookup and then present two different ids for one address. The store is
/// right to refuse the second — `upsert_user` holds a lock and rejects an email
/// already held by another id, which is the invariant that keeps
/// `find_user_by_email` unambiguous — but the caller's question was wrong. It
/// asked "may I create this user", when what it needed to know was "who owns
/// this address now".
///
/// So a `Conflict` here is read as "somebody else materialized it", and the
/// winner is returned. That is what makes the operation idempotent under
/// concurrency, which is what both callers' doc comments already claim.
///
/// Issue #1833. On a desktop first boot three requests raced
/// [`local_owner_record`]; one won and two were refused 16ms later, and
/// `graphql::auth` turns that refusal into `GatesRefused` — so the console
/// reported the healthy host it was talking to as "Unreachable" and offered no
/// way in. A restart fixed it permanently, because by then the record existed.
///
/// **Only `Conflict` is adopted.** Every other error still propagates, so the
/// store-outage refusal that `graphql::auth` deliberately makes fatal stays
/// fatal: a store that cannot be read must not read as "this user is fine".
///
/// A `Conflict` with nothing behind it returns the original error rather than
/// retrying. That means the winner was deleted between the write and the
/// re-read, and looping on it would spin against a store doing something this
/// function has no business papering over.
pub(super) async fn insert_or_adopt(
    runtime: &CompanyRuntime,
    user: UserRecord,
) -> Result<UserRecord, OpenCompanyError> {
    let id = runtime.id();
    match runtime.users().upsert_user(id, &user).await {
        Ok(()) => Ok(user),
        Err(OpenCompanyError::Conflict(conflict)) => {
            match runtime.users().find_user_by_email(id, &user.email).await? {
                Some(winner) => {
                    tracing::debug!(
                        company = %id,
                        email = %user.email,
                        "adopted the user record that won the materialization race"
                    );
                    Ok(winner)
                }
                None => Err(OpenCompanyError::Conflict(conflict)),
            }
        }
        Err(other) => Err(other),
    }
}

/// Returns the existing user for `email`, or materializes one from their
/// eligibility.
///
/// This is where an invite becomes an account. Redemption is not a separate
/// flow with its own credential: first login and Nth login are the same code
/// path, which is what keeps the two from drifting apart.
///
/// `pub(crate)` so [`server::sso`](crate::server::sso) claims-or-logs-in the
/// admin through the same materialization: an SSO redemption whose `sub` has
/// already been proven a standing admin is a login, and creating the account on
/// first use is the claim. Reusing this keeps "first SSO login mints a
/// passwordless admin, Nth logs the same account in" identical to the magic-link
/// path rather than a second, drifting one.
pub(crate) async fn upsert_from_eligibility(
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
        avatar: None,
        role,
        status: UserStatus::Active,
        password_hash: None,
        must_change_password: false,
        created_at_millis: now,
        last_seen_at_millis: Some(now),
        updated_at_millis: now,
    };
    let user = insert_or_adopt(runtime, user).await?;
    // Mark any real invite as redeemed. A manifest-bootstrapped admin has no
    // invite record, so this is a no-op for them.
    if let Some(mut invite) = runtime.users().find_invite_by_email(id, email).await? {
        invite.accepted_at_millis = Some(now);
        runtime.users().upsert_invite(id, &invite).await?;
    }
    Ok(user)
}

/// The single user of a company that has no sign-in, materialized on first use.
///
/// `none` mode has no eligibility question to answer — there is no invite list,
/// no bootstrap list, and no second person to admit — so this deliberately does
/// **not** go through [`eligibility`]. It is the one place a user record is
/// created without anyone proving anything, and it can be, because the mode's
/// premise is that whoever reaches this host is its owner.
///
/// Idempotent: the record is keyed by [`LoginIdentity::Local`], so every request
/// after the first is a read. It is minted as an `Admin` because the person at
/// the machine owns the company, and there is nobody for a lesser role to be
/// distinguished from.
pub(crate) async fn local_owner_record(
    runtime: &CompanyRuntime,
) -> Result<UserRecord, OpenCompanyError> {
    let id = runtime.id();
    let key = LoginIdentity::Local.key();
    if let Some(user) = runtime.users().find_user_by_email(id, &key).await? {
        return Ok(user);
    }
    let now = now_millis();
    let user = UserRecord {
        id: generate_id(),
        email: key,
        display_name: None,
        avatar: None,
        role: UserRole::Admin,
        status: UserStatus::Active,
        // No password and no way to set one: `auth/password` refuses outside
        // email mode, and there is no login for a password to guard.
        password_hash: None,
        must_change_password: false,
        created_at_millis: now,
        last_seen_at_millis: Some(now),
        updated_at_millis: now,
    };
    insert_or_adopt(runtime, user).await
}

// ---------------------------------------------------------------------------
// Session minting
// ---------------------------------------------------------------------------

/// Writes a session for `user` and returns the plaintext token, once.
///
/// The single minting path for both carriers. A device differs only in its
/// [`SessionKind`], its label, and how long it lives — everything that makes a
/// session safe (hash-only storage, the sign-in stamp, the opportunistic purge)
/// is here so neither caller can skip it.
pub(crate) async fn create_session(
    runtime: &CompanyRuntime,
    user: &UserRecord,
    kind: SessionKind,
    label: Option<String>,
    user_agent: Option<String>,
) -> crate::Result<String> {
    let company = runtime.id();
    let now = now_millis();
    let plaintext = token::mint_session_token(&token::OsTokens);
    let ttl = match kind {
        SessionKind::Browser => token::SESSION_TTL_MILLIS,
        SessionKind::Device => token::DEVICE_TTL_MILLIS,
    };
    let session = SessionRecord {
        id: generate_id(),
        // Only the hash is persisted; the plaintext is returned to exactly one
        // caller and is never written down.
        token_hash: token::sha256_hex(&plaintext),
        user_id: user.id.clone(),
        created_at_millis: now,
        expires_at_millis: now + ttl,
        user_agent,
        kind,
        label,
    };
    runtime.sessions().create(company, &session).await?;

    // Record the sign-in on the user. Every session is minted here — link,
    // password and device alike — so this is the one place that makes
    // `last_seen` mean "last signed in" rather than "joined". It is
    // deliberately not updated per request: that would be a store write on
    // every authenticated call, and knowing someone signed in an hour ago is
    // not worth that.
    let mut signed_in = user.clone();
    signed_in.last_seen_at_millis = Some(now);
    signed_in.updated_at_millis = now;
    let _ = runtime.users().upsert_user(company, &signed_in).await;

    // Opportunistic cleanup on a cold path, so no background task is needed.
    let _ = runtime.sessions().purge_expired(company, now).await;
    let _ = runtime.login_codes().purge_expired(company, now).await;

    Ok(plaintext)
}

/// Mints a browser session for `user` and renders it in the carrier they asked
/// for: a `Set-Cookie` by default, or the token in the body for a client that
/// cannot receive a cookie at all (see [`cookie::SESSION_CARRIER_HEADER`]).
///
/// One choke point for every browser login path — magic link, password, the
/// first-admin claim, wallet and SSO auto-login — so a carrier is added once
/// rather than five times, and no path can acquire one without the
/// session-minting invariants in [`create_session`].
///
/// `pub(crate)` so [`server::sso`](crate::server::sso) mints its session the
/// same way: an SSO redemption is a login like any other, and routing it through
/// this function is what keeps it honoring the cross-origin header carrier
/// without re-deriving the cookie/header split.
pub(crate) async fn mint_session(
    state: &AppState,
    runtime: &CompanyRuntime,
    user: &UserRecord,
    headers: &HeaderMap,
) -> Result<Response, crate::server::Rejection> {
    let company = runtime.id();
    // A company whose id cannot safely name a cookie cannot hold a session;
    // refuse rather than emit a header its id could have chosen attributes for.
    // Checked for both carriers, not just the cookie: `session_header_value`
    // enforces the same rule, so failing here keeps the refusal in one place
    // and keeps the two carriers addressable by exactly the same set of ids.
    let Some(name) = cookie::session_cookie_name(company) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "this company's id cannot carry a session cookie".to_string(),
        ))
        .into_response()
        .into());
    };
    let plaintext = create_session(
        runtime,
        user,
        SessionKind::Browser,
        None,
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.chars().take(200).collect()),
    )
    .await?;

    // The header carrier hands the token to the client and sets **no** cookie.
    // Setting both would leave one session reachable two ways, and the cookie
    // half would be a third-party cookie that some browsers keep and others
    // drop — so whether logging out cleared the session would depend on the
    // browser. One session, one carrier.
    if cookie::wants_header_carrier(headers) {
        let Some(session) = cookie::session_header_value(company, &plaintext) else {
            return Err(ApiError(OpenCompanyError::InvalidRequest(
                "this company's id cannot carry a session header".to_string(),
            ))
            .into_response()
            .into());
        };
        return Ok(Json(SignInResult {
            user: me_result(company, user),
            session: Some(session),
        })
        .into_response());
    }

    let insecure = !state.config().host_base_url().starts_with("https://");
    let set = cookie::set_cookie(
        &name,
        &plaintext,
        token::SESSION_TTL_MILLIS / 1000,
        insecure,
    );
    let body = Json(SignInResult {
        user: me_result(company, user),
        session: None,
    });
    Ok(([(header::SET_COOKIE, set)], body).into_response())
}

/// Mints a browser session for `user` and returns its `Set-Cookie` header value,
/// split out of [`mint_session`] so a handler with its own response body —
/// first-run setup's `AppliedDto` — can sign the operator in without having to
/// emit [`mint_session`]'s `SignInResult`. The cookie's attributes still come
/// from [`cookie::set_cookie`] and the value from [`create_session`], so a
/// session minted here is indistinguishable from one minted by any other login
/// path.
///
/// Cookie-only, with no header-carrier branch, because its one caller — the SSO
/// bootstrap setup apply — is always same-origin: the platform SSO link lands
/// the operator in the host's own console (where the console's `needsCarriedSession`
/// is false), and the apply is an ordinary `post` that never asks for the header
/// carrier. A cross-origin login still receives the header carrier through
/// [`mint_session`], which every cross-origin sign-in path already routes through.
pub(crate) async fn mint_session_cookie(
    state: &AppState,
    runtime: &CompanyRuntime,
    user: &UserRecord,
    headers: &HeaderMap,
) -> Result<String, crate::server::Rejection> {
    let company = runtime.id();
    let Some(name) = cookie::session_cookie_name(company) else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "this company's id cannot carry a session cookie".to_string(),
        ))
        .into_response()
        .into());
    };
    let plaintext = create_session(
        runtime,
        user,
        SessionKind::Browser,
        None,
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.chars().take(200).collect()),
    )
    .await?;
    let insecure = !state.config().host_base_url().starts_with("https://");
    Ok(cookie::set_cookie(
        &name,
        &plaintext,
        token::SESSION_TTL_MILLIS / 1000,
        insecure,
    ))
}

fn me_result(company: &CompanyId, user: &UserRecord) -> MeResult {
    MeResult {
        id: user.id.clone(),
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        avatar: user.avatar.clone(),
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
) -> Result<Json<RequestCodeResult>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    if let Some(refusal) = wrong_mode_for_email(&runtime) {
        return Err(refusal.into());
    }
    let email = normalize_email(&body.email);
    let now = now_millis();

    let eligible = eligibility(state.config(), &runtime, &email, now).await?;
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
            .await?
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
        .await?;
    runtime.login_codes().create(runtime.id(), &record).await?;

    // Deliver. A send failure must not change the response — it would report
    // that the address exists.
    let delivered = deliver_code(
        &state,
        &runtime,
        &email,
        &plaintext,
        body.redirect.as_deref(),
    )
    .await;

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
///
/// Shared with the admin invite route (issue #584) so "can this host mail at
/// all" keeps one answer. It matters there for a reason beyond tidiness: an
/// invite mailed through some *other* transport would invite someone into a
/// dead flow, because the magic link they then ask for is gated on exactly
/// this predicate. One transport, one truthful answer.
pub(crate) fn mail_transport_wired(state: &AppState) -> bool {
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
pub(crate) fn echoes_code_in_response(state: &AppState) -> bool {
    state.config().is_local_only() && !mail_transport_wired(state)
}

/// The safe subset of a URL fragment a mailed login link may carry.
///
/// Only the fragment part of a link is ever client-supplied, and a value that
/// cannot be honoured is dropped rather than refused — a malformed redirect
/// must not block sign-in. The set is deliberately small: fragment characters
/// that route the console (`#/company`), with nothing that could break the
/// link out of a mail client's linkification (`@`, whitespace, control
/// characters).
fn redirect_fragment(redirect: &str) -> Option<String> {
    let safe = redirect.starts_with('#')
        && redirect.len() <= 128
        && redirect.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'#' | b'/' | b'?' | b'&' | b'=' | b'-' | b'_' | b'.')
        });
    safe.then(|| redirect.to_string())
}

/// Mails the magic link. Returns whether it was actually sent.
///
/// `redirect`, when present, is appended to the link so the console lands on
/// the fragment the requester asked for (setup's `#/company`, for example)
/// rather than its default view. Sanitized by [`redirect_fragment`]: an
/// invalid value means the link is mailed without it, never refused.
async fn deliver_code(
    state: &AppState,
    runtime: &CompanyRuntime,
    email: &str,
    code: &str,
    redirect: Option<&str>,
) -> bool {
    // Asked through the shared predicate so "can this host mail at all" has one
    // answer: the throttle and the dev echo both branch on it, and a second
    // spelling here is how those three drift apart.
    if !mail_transport_wired(state) {
        return false;
    }
    // A plain username has no mailbox: a login on a host with no mail that
    // later gained one. Nothing to send to, so nothing is sent — the person
    // signs in with their password, as they always did.
    if LoginIdentity::parse(email).mailbox().is_none() {
        return false;
    }
    let connections = state.connections();
    let (Some(sender), Some(creds)) = (&connections.mail, &connections.mail_credentials) else {
        return false;
    };
    let base = state.config().host_base_url();
    let mut link = format!("{base}/login?company={}&code={code}", runtime.id().as_ref());
    if let Some(fragment) = redirect.and_then(redirect_fragment) {
        link.push_str(&fragment);
    }
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
            // `error!`, not `warn!`: without `RUST_LOG` the default
            // `EnvFilter` shows errors only, and this is the sole record of why
            // nobody can sign in.
            tracing::error!(company = %runtime.id(), "login mail failed: {err}");
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
) -> Result<Response, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    if let Some(refusal) = wrong_mode_for_email(&runtime) {
        return Err(refusal.into());
    }
    let now = now_millis();
    // Single use is the store's guarantee, not a check here: `consume` matches
    // and marks atomically, so two requests racing on one link cannot both win.
    let consumed = runtime
        .login_codes()
        .consume(runtime.id(), &token::sha256_hex(&body.code), now)
        .await?;
    let Some(code) = consumed else {
        return Err(invalid_login().into());
    };

    // The address comes from the *code*, never from the request: otherwise
    // anyone holding any valid link could name whoever they liked.
    let Some(role) = eligibility(state.config(), &runtime, &code.email, now).await? else {
        // Eligibility can lapse between mailing and clicking.
        return Err(invalid_login().into());
    };
    let user = upsert_from_eligibility(&runtime, &code.email, role, now).await?;
    mint_session(&state, &runtime, &user, &headers).await
}

/// `POST …/auth/claim` — the first person in picks the admin login and sets
/// its password, then is signed in.
///
/// The way into a company nobody has joined yet, from the screen the person is
/// actually looking at. Every other first sign-in needed a credential the host
/// might not be able to deliver — a mailed link with no transport, or a shell
/// command on the host — and the console dead-ended on a form that could not
/// work (issue #1718 was the CLI half of that; this is the console half).
///
/// Open **only while the company has no users**, and closed for good after.
/// That is what keeps a first-come claim from being an open door: it admits
/// exactly one person, and after them the only way in is the roster. Where the
/// deployment already named its first admin — a manifest `[users].admins`
/// entry or `OPENCOMPANY_ADMIN_EMAIL` — only that address may claim, so a
/// stranger who reaches a provisioned tenant before its owner cannot take it.
///
/// The refusals are specific rather than the generic `invalid_login`. The
/// generic-failure rule protects who is *on* the roster, and this route only
/// answers while the roster is empty — the one fact it discloses, that nobody
/// has joined, is the same one `auth/config` publishes as `claimable`.
async fn claim_first_admin(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ClaimFirstAdmin>,
) -> Result<Response, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    if let Some(refusal) = wrong_mode_for_email(&runtime) {
        return Err(refusal.into());
    }
    let standing = bootstrap_admins(state.config(), &runtime).await?;
    let claimed = super::bootstrap::claim_first_admin(
        runtime.users(),
        runtime.id(),
        &standing,
        &body.email,
        &body.password,
    )
    .await?;
    let user = match claimed {
        Ok(user) => user,
        Err(super::bootstrap::ClaimRefusal::AlreadyClaimed) => {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "this company already has an admin — sign in instead",
                    "code": "already_claimed",
                })),
            )
                .into_response()
                .into());
        }
        Err(super::bootstrap::ClaimRefusal::NotTheNamedAdmin) => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "this host already names its first admin — claim it with that address",
                    "code": "not_the_named_admin",
                })),
            )
                .into_response()
                .into());
        }
    };
    tracing::info!(company = %runtime.id(), "first admin claimed");
    mint_session(&state, &runtime, &user, &headers).await
}

/// `POST …/auth/login` — exchange an email and password for a session.
async fn login_password(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginPassword>,
) -> Result<Response, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    if let Some(refusal) = wrong_mode_for_email(&runtime) {
        return Err(refusal.into());
    }
    let email = normalize_email(&body.email);
    let now = now_millis();

    let user = runtime
        .users()
        .find_user_by_email(runtime.id(), &email)
        .await?;

    // Every path with no hash to check burns equivalent work first, so an
    // unknown address costs the same wall-clock as a wrong password.
    let Some(user) = user else {
        password::dummy_verify(&body.password);
        return Err(invalid_login().into());
    };
    if user.status != UserStatus::Active {
        password::dummy_verify(&body.password);
        return Err(invalid_login().into());
    }
    let Some(hash) = user.password_hash.as_deref() else {
        // Magic-link-only account.
        password::dummy_verify(&body.password);
        return Err(invalid_login().into());
    };
    if !password::verify(&body.password, hash) {
        return Err(invalid_login().into());
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
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
) -> Result<Response, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    // Nothing to revoke where nothing was minted. A `none`-mode principal is
    // resolved from the request, not from a stored session, so "log out" has no
    // meaning there and quietly succeeding would tell the console it had signed
    // someone out when the very next request is authenticated again.
    if let Some(refusal) = wrong_mode_for_login(&runtime) {
        return Err(refusal.into());
    }
    let insecure = !state.config().host_base_url().starts_with("https://");
    let Some(name) = cookie::session_cookie_name(runtime.id()) else {
        return Err(no_session().into());
    };

    // Revoke server-side when the cookie names a real session; clearing the
    // cookie alone would leave a working token in whatever else holds it.
    if let Some(user) = current_user(&headers, &state, runtime.id(), peer).await
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
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
) -> Result<Json<MeResult>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    let Some(principal) = current_user(&headers, &state, runtime.id(), peer).await else {
        return Err(no_session().into());
    };
    let user = runtime
        .users()
        .get_user(runtime.id(), &principal.user_id)
        .await?
        .ok_or_else(no_session)?;
    Ok(Json(me_result(runtime.id(), &user)))
}

/// What a person may change about themselves.
///
/// Both fields are **double options**, the same three-state contract the
/// team-edit routes use:
///
/// | body | parses as | means |
/// |---|---|---|
/// | `{}` | `None` | leave it alone |
/// | `{"avatar": null}` | `Some(None)` | back to the default |
/// | `{"avatar": "tiny:teal"}` | `Some(Some(…))` | this one |
///
/// Collapsing the first two would make every partial save erase the field it
/// did not mention, which on a two-field profile form means saving a name wipes
/// the face.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditMe {
    #[serde(default, deserialize_with = "crate::server::ops::team::double_option")]
    display_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::server::ops::team::double_option")]
    avatar: Option<Option<String>>,
}

/// `PATCH …/auth/me` — change your own name or face.
///
/// # Why this is not the admin route
///
/// `PATCH …/users/{id}` can already set somebody's `displayName`, and it is
/// admin-only — correct for an admin making a roster of raw addresses legible,
/// and useless for the case this route exists for. Naming yourself and choosing
/// your own face are not administrative acts, and gating them behind an admin
/// would mean a member's own identity in the company is something they have to
/// ask for. So this route authorises on **being** the user rather than on a
/// role: there is no `user_id` in the path at all, which is what makes it
/// impossible to point at somebody else.
///
/// Every sign-in mode has it, including `none`: the single local owner of a
/// company with no sign-in is still a person with a name and a face.
async fn edit_me(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<EditMe>,
) -> Result<Json<MeResult>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    let Some(principal) = current_user(&headers, &state, runtime.id(), peer).await else {
        return Err(no_session().into());
    };
    // A temporary password is for replacing itself, not for spending on the
    // account's public name or face. An admin who reset a password knows the
    // value, so a session opened with one must not be able to change what the
    // rest of the company sees before the user has chosen a private one; the
    // deliberately-public `GET` stays open so they can still read who they are
    // and land on the set-password route. Same refusal
    // [`refuse_until_password_changed`](crate::server::platform_auth::refuse_until_password_changed)
    // returns for the routes that go through the extractors.
    if principal.must_change_password {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "set a new password before continuing",
                "code": "password_change_required",
            })),
        )
            .into_response()
            .into());
    }
    // The avatar resolve is the one slow part of this handler — a workspace
    // read of an uploaded image — and `upsert_user` below replaces the *entire*
    // record with whatever `get_user` returned. Resolving the face before that
    // read keeps the record we persist from being loaded ahead of a long await:
    // a `status`, `role` or `password_hash` an admin changed while the image was
    // being looked up is read, not resurrected. (The residual read-to-write gap
    // is systemic — every user write in this store replaces the whole record —
    // and closing it for good is a store-level partial update, not something a
    // route can do alone.)
    let avatar = match body.avatar {
        // Field absent — leave the face alone. `Some` here means "the body spoke
        // about the avatar"; the inner value is the stored reference (`None`
        // clears back to the hashed default).
        None => None,
        Some(inner) => Some(
            match inner
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
            {
                // Validated against what this host holds before it is stored — the
                // value ends up in an `src=` on every surface that draws this
                // person's face. See `crate::company::avatar`.
                Some(value) => Some(
                    crate::company::avatar::resolve(
                        runtime.workspace().as_ref(),
                        runtime.id(),
                        &value,
                    )
                    .await
                    .map_err(crate::server::Rejection::from)?,
                ),
                None => None,
            },
        ),
    };
    let mut user = runtime
        .users()
        .get_user(runtime.id(), &principal.user_id)
        .await
        .map_err(crate::server::Rejection::from)?
        .ok_or_else(no_session)?;

    // A blank name is not a name: an emptied field is the person asking for the
    // derived one back, which is the same intent `null` carries, so the two
    // normalize to one stored state rather than to a name that renders as a gap.
    if let Some(name) = body.display_name {
        let name = name
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        if let Some(name) = &name {
            super::validate_display_name(name)?;
        }
        user.display_name = name;
    }
    if let Some(avatar) = avatar {
        user.avatar = avatar;
    }
    user.updated_at_millis = now_millis();
    runtime
        .users()
        .upsert_user(runtime.id(), &user)
        .await
        .map_err(crate::server::Rejection::from)?;
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
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<SetPassword>,
) -> Result<Json<MeResult>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    // A password is an alternative to a mailbox round trip, so it only exists
    // where the mailbox does. In wallet mode the key is the credential; in
    // `none` mode there is nobody to distinguish from anybody.
    if let Some(refusal) = wrong_mode_for_email(&runtime) {
        return Err(refusal.into());
    }
    let Some(principal) = current_user(&headers, &state, runtime.id(), peer).await else {
        return Err(no_session().into());
    };
    let mut user = runtime
        .users()
        .get_user(runtime.id(), &principal.user_id)
        .await?
        .ok_or_else(no_session)?;

    password::validate(&body.password, &user.email)?;
    let hash = password::hash(&token::OsTokens, &body.password)?;
    let now = now_millis();
    user.password_hash = Some(hash);
    // Whatever prompted the change is now satisfied.
    user.must_change_password = false;
    user.updated_at_millis = now;
    runtime.users().upsert_user(runtime.id(), &user).await?;

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

// ---------------------------------------------------------------------------
// Wallet sign-in
// ---------------------------------------------------------------------------

/// `POST …/auth/wallet/challenge` — mint a nonce for a wallet to sign.
///
/// Always `200` with a challenge, for every syntactically valid address — the
/// generic-failure rule, one step earlier than the magic link needs it. A route
/// that answered "no challenge for you" would be a membership oracle for the
/// company's wallet roster, and a wallet address is public, so an attacker
/// probing one has nothing to lose.
///
/// An **ineligible** address gets a challenge that is never persisted. It is
/// well-formed, it is signable, and answering it fails with the same
/// `invalid_login` a bad signature gets. That also keeps an anonymous caller
/// from filling the code table by naming addresses at random.
async fn wallet_challenge(
    company: PublicCompany,
    State(state): State<AppState>,
    Json(body): Json<wallet::ChallengeRequest>,
) -> Result<Json<wallet::ChallengeResult>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    if let Some(refusal) = wrong_mode_for_wallet(&runtime) {
        return Err(refusal.into());
    }
    let now = now_millis();

    // A malformed address cannot be told apart from an unknown one: both get a
    // challenge shaped exactly like a real one, minted against the address as
    // the caller typed it.
    let Some(address) = wallet::parse_wallet_address(&body.address) else {
        return Ok(Json(wallet::unpersisted_challenge(
            runtime.id(),
            &token::OsTokens,
            &normalize_wallet(&body.address),
            now,
        )));
    };

    let identity = LoginIdentity::Wallet(address.clone()).key();
    let eligible = eligibility(state.config(), &runtime, &identity, now).await?;
    if eligible.is_none() {
        return Ok(Json(wallet::unpersisted_challenge(
            runtime.id(),
            &token::OsTokens,
            &address,
            now,
        )));
    }

    wallet::issue_challenge(&runtime, &token::OsTokens, &address, now)
        .await
        .map(Json)
        .map_err(|e| ApiError(e).into_response().into())
}

/// `POST …/auth/wallet/verify` — answer a challenge and receive a session.
///
/// The same three calls the magic-link path makes once identity is settled —
/// [`eligibility`], [`upsert_from_eligibility`], [`mint_session`] — so first
/// sign-in and Nth sign-in are one code path and a wallet gets no privilege a
/// mailed link would not have given the same person.
async fn wallet_verify(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<wallet::VerifyRequest>,
) -> Result<Response, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    if let Some(refusal) = wrong_mode_for_wallet(&runtime) {
        return Err(refusal.into());
    }
    let now = now_millis();

    let Some(address) = wallet::verify_challenge(&runtime, &body, now).await else {
        return Err(invalid_login().into());
    };

    // The address comes from the challenge record, never from this request, so
    // eligibility is re-checked against the wallet that actually signed.
    let identity = LoginIdentity::Wallet(address).key();
    let Some(role) = eligibility(state.config(), &runtime, &identity, now).await? else {
        // Eligibility can lapse between the challenge and the answer.
        return Err(invalid_login().into());
    };
    let user = upsert_from_eligibility(&runtime, &identity, role, now).await?;
    mint_session(&state, &runtime, &user, &headers).await
}

// ---------------------------------------------------------------------------
// What this company's sign-in screen looks like
// ---------------------------------------------------------------------------

/// What the console needs before it can draw a sign-in screen.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthConfigResult {
    /// `email` | `wallet` | `none`.
    mode: &'static str,
    /// What this company calls itself, so the sign-in screen can name what the
    /// person is about to hand a credential to. The manifest's display name,
    /// falling back to the company id; never absent, never empty.
    name: String,
    /// Whether a password may be offered alongside the magic-link form. Only
    /// ever true in `email` mode.
    passwords: bool,
    /// Whether a magic link asked for here reaches a mailbox — this host has a
    /// mail transport wired. False means the console draws no link form at
    /// all: a password is the only sign-in, and the screen says so rather
    /// than sending someone to wait for a message no process here will send.
    magic_link: bool,
    /// Whether nobody has joined this company yet, so the first person in may
    /// pick the admin login and its password (`POST …/auth/claim`). Only ever
    /// true in `email` mode, and only until the first user exists.
    claimable: bool,
}

/// `GET …/auth/config` — the sign-in mode this company uses.
///
/// Unauthenticated by construction, like every other login route: the console
/// asks this *before* anyone has a credential, because it cannot draw the screen
/// otherwise. Publishing the mode discloses nothing about who is on the roster —
/// it is a property of the deployment, and a caller can already infer it from
/// which routes answer.
///
/// The console must branch on this rather than on which routes 404, so that a
/// company with no sign-in renders "open the desktop app" instead of an email
/// box that can never work.
///
/// It carries the company's display name for the same reason it carries the
/// mode: the console has to draw the screen before it can authenticate, and a
/// sign-in page that cannot name what it is a sign-in *to* asks for a credential
/// without saying who is receiving it.
async fn auth_config(
    company: PublicCompany,
    State(state): State<AppState>,
) -> Json<AuthConfigResult> {
    let mode = company.runtime.auth_mode();
    Json(AuthConfigResult {
        mode: mode.as_str(),
        // The one place a person can confirm *which* company they are signing
        // in to. Every tenant is its own deployment on its own URL, so the host
        // knows this for certain — and before this it told the console nothing,
        // leaving the sidebar (after sign-in) as the first thing to name the
        // company (issue #1334). Disclosing it discloses no membership: it is a
        // property of the deployment, exactly like `mode`.
        name: company.runtime.display_name().await,
        passwords: mode.uses_email(),
        // The same predicate `request_code` itself delivers through, asked
        // rather than restated: whether the console draws the form and whether
        // the code goes anywhere must never be two separate opinions. The
        // loopback echo (`dev_code`) is deliberately *not* counted: it is a
        // developer convenience on the API, and a sign-in screen that offers
        // "email me a link" on a host with no mail is the confusion this
        // field exists to remove.
        magic_link: mail_transport_wired(&state),
        // A store read on an unauthenticated route, but a cold one: the
        // console asks once per sign-in screen, and it is the difference
        // between a first visitor being offered a way in and being offered a
        // form nobody can pass.
        claimable: mode.uses_email()
            && super::bootstrap::is_unclaimed(company.runtime.users(), company.runtime.id())
                .await
                .unwrap_or(false),
    })
}

/// The user behind this request's session cookie, if any.
///
/// `peer` reaches [`local_owner`](crate::server::graphql::auth::local_owner)'s
/// loopback-peer gate for `none` mode, same as
/// [`CompanyAuth`](crate::server::platform_auth::CompanyAuth) and the GraphQL
/// handler. Pass `None` only where the caller genuinely has no socket to name.
pub(crate) async fn current_user(
    headers: &HeaderMap,
    state: &AppState,
    company: &CompanyId,
    peer: Option<std::net::SocketAddr>,
) -> Option<UserPrincipal> {
    match resolve_principal(headers, state, Some(company), peer).await {
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
                // A bootstrapped admin is eligible because configuration says
                // so; nothing was ever mailed to them, and no row exists to
                // stamp if it were.
                notified_at_millis: None,
            }
        })
        .collect())
}
