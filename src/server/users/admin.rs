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
use crate::app::config::AuthMode;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::{
    InviteRecord, LoginIdentity, UserRecord, UserRole, UserStatus, decode_wallet_address,
    generate_id, normalize_email, normalize_wallet, now_millis,
};
use crate::server::error::ApiError;
use crate::server::graphql::auth::UserPrincipal;
use crate::server::ops::mailer::OutboundEmail;
use crate::server::users::routes::{
    current_user, load_manifest, mail_transport_wired, manifest_admin_invites,
};
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

/// Refuses a user-administration route on a company that has no sign-in.
///
/// `none` mode admits exactly one person — whoever is at the machine — and has
/// no way to tell a second one apart. Every route that would add, re-role,
/// suspend or re-credential somebody is therefore refused outright rather than
/// left to write records that can never be reached. Listing stays available:
/// showing the one local owner is honest, and an empty screen would not be.
///
/// `Option`, not `Result<(), Response>`, for the reason given on
/// [`wrong_mode_for_email`](crate::server::users::routes::wrong_mode_for_email).
fn wrong_mode_for_admin(runtime: &CompanyRuntime) -> Option<Response> {
    crate::server::users::routes::wrong_mode_for_login(runtime)
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
///
/// Shared beyond this module (issue #343: the team budget writes) so "who may
/// administer this company" is decided in exactly one place. Note what it
/// resolves through: [`current_user`] yields a principal only for a **human**
/// session, so a platform/tenant bearer — a machine credential — is not an
/// admin here and is refused as unauthenticated. That is the same "no operator
/// break-glass" doctrine this module's header states, and it is what makes the
/// attribution recorded by callers a real person rather than a token.
pub(crate) async fn require_admin(
    headers: &HeaderMap,
    state: &AppState,
    runtime: &CompanyRuntime,
    peer: Option<std::net::SocketAddr>,
) -> Result<UserPrincipal, crate::server::Rejection> {
    let principal = current_user(headers, state, runtime.id(), peer)
        .await
        .ok_or_else(unauthorized)?;
    if !principal.may_administer() {
        return Err(forbidden().into());
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
    /// The face this person chose (`docs/spec/runtime/avatars.md`), absent when
    /// they have not chosen one — which the console draws as the mascot it
    /// hashes from their id.
    ///
    /// Readable here, but **not writable**: unlike `displayName`, which an admin
    /// may set so a roster of raw addresses can be made legible, a person's own
    /// face is theirs to pick. `PATCH …/auth/me` is where it is written, by the
    /// person wearing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar: Option<String>,
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
            avatar: u.avatar,
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
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
) -> Result<Json<Vec<UserSummary>>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    require_admin(&headers, &state, &runtime, peer).await?;
    let users = runtime.users().list_users(runtime.id()).await?;
    Ok(Json(users.into_iter().map(UserSummary::from).collect()))
}

/// `GET …/users/invites` — outstanding invites, including manifest admins.
async fn list_invites(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
) -> Result<Json<Vec<InviteRecord>>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    require_admin(&headers, &state, &runtime, peer).await?;
    let now = now_millis();
    let mut invites = runtime.users().list_invites(runtime.id()).await?;
    // Manifest admins are eligible without an invite record. Showing only the
    // stored ones would render a list that contradicts who can actually log in.
    let stored: Vec<String> = invites.iter().map(|i| i.email.clone()).collect();
    let users = runtime.users().list_users(runtime.id()).await?;
    let synthetic = manifest_admin_invites(state.config(), &runtime, now).await?;
    for invite in synthetic {
        let already_a_user = users.iter().any(|u| u.email == invite.email);
        if !stored.contains(&invite.email) && !already_a_user {
            invites.push(invite);
        }
    }
    Ok(Json(invites))
}

/// Who to invite.
///
/// The identifier field follows the company's [`AuthMode`]: `email` in email
/// mode, `wallet` in wallet mode. Two named fields rather than one polymorphic
/// one, because they are normalized by different rules — an address is
/// lowercased, a base58 key must not be — and a single field would have to
/// guess which rule applies to what was typed.
#[derive(Debug, Default, Deserialize)]
struct InviteBody {
    #[serde(default)]
    email: String,
    #[serde(default)]
    wallet: String,
    #[serde(default)]
    role: UserRole,
}

impl InviteBody {
    /// The [`LoginIdentity`] key this invite grants, validated for `mode`.
    ///
    /// The error is prosumer-facing and safe to render: this route is
    /// admin-authenticated and the caller supplied the value, so unlike the
    /// login routes there is nothing here they could learn from a specific
    /// message that they did not already know.
    fn identity(&self, mode: AuthMode) -> Result<String, OpenCompanyError> {
        // The field the mode does not read is refused rather than silently
        // dropped: an admin who filled in both fields, or the one the company
        // does not use, believes they invited something they did not.
        match mode {
            AuthMode::Email if !self.wallet.trim().is_empty() => {
                Err(OpenCompanyError::InvalidRequest(
                    "this company signs in by email, not wallet — leave `wallet` empty".to_string(),
                ))
            }
            AuthMode::Wallet if !self.email.trim().is_empty() => {
                Err(OpenCompanyError::InvalidRequest(
                    "this company signs in by wallet, not email — leave `email` empty".to_string(),
                ))
            }
            AuthMode::Email => {
                let email = normalize_email(&self.email);
                if email.is_empty() || !email.contains('@') {
                    return Err(OpenCompanyError::InvalidRequest(
                        "that doesn't look like an email address".to_string(),
                    ));
                }
                Ok(LoginIdentity::Email(email).key())
            }
            AuthMode::Wallet => {
                let wallet = normalize_wallet(&self.wallet);
                // Decoded, not merely non-empty: an address that cannot be
                // decoded can never verify a signature, so accepting it would
                // write a grant that is unusable by construction and looks live
                // on the roster.
                decode_wallet_address(&wallet)?;
                Ok(LoginIdentity::Wallet(wallet).key())
            }
            // Unreachable — the route refuses before asking — but answered
            // rather than panicked, so a later caller cannot turn a missed guard
            // into a crash.
            AuthMode::None => Err(OpenCompanyError::InvalidRequest(
                "this company has no sign-in, so nobody can be invited".to_string(),
            )),
        }
    }
}

/// What actually happened to the invite mail (issue #584).
///
/// The invite record is written either way — this reports delivery, it does not
/// gate the grant. It is reported to the caller rather than swallowed because
/// this route, unlike `auth/request`, has no enumeration oracle to protect: it
/// is admin-authenticated and the caller typed the address in themselves, so
/// there is nothing here they could learn that they did not already supply.
/// A success toast over a mail that never left is the entire bug in the issue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InviteDelivery {
    /// The transport accepted the message.
    Sent,
    /// This host has no mail transport wired, so nothing was attempted.
    NoTransport,
    /// A transport was wired and refused the message.
    Failed,
    /// This identity has no mailbox to write to — a wallet invite. Not a
    /// failure and not a missing transport: there was never a message to send,
    /// and the console says so rather than implying an outage.
    NoMailbox,
}

/// The invite, plus what happened to the mail.
///
/// `flatten` keeps every existing [`InviteRecord`] field at the top level, so
/// this is additive on the wire — an older client reading `id` / `email` /
/// `role` sees exactly what it saw before.
#[derive(Debug, Serialize)]
struct InviteResult {
    #[serde(flatten)]
    invite: InviteRecord,
    delivery: InviteDelivery,
}

/// Mails an invited address, returning what happened.
///
/// The mail carries **no credential** — no code, no token, not even the invite
/// id. The recipient still goes through `auth/request` like anyone else, so the
/// roster remains the only gate and this stays a notification. That is why it
/// is safe to send to an address a human typed, possibly wrongly: the worst
/// case is a stranger learning that a company they cannot enter exists.
///
/// The transport is the **host-level** one — the same seam the magic link uses,
/// asked through the same predicate. Falling back to the company's own `__smtp`
/// secret was considered and rejected: it would mail an invite from a host that
/// cannot then mail the sign-in link, inviting someone into a dead flow.
async fn send_invite_mail(
    state: &AppState,
    runtime: &CompanyRuntime,
    invite: &InviteRecord,
    inviter: &str,
) -> InviteDelivery {
    // A wallet identity has no mailbox. Asked of the identity rather than of the
    // mode, so this cannot be reached with an address that only looks like one:
    // `LoginIdentity::mailbox` is `None` for everything that is not an email.
    if LoginIdentity::parse(&invite.email).mailbox().is_none() {
        return InviteDelivery::NoMailbox;
    }
    if !mail_transport_wired(state) {
        return InviteDelivery::NoTransport;
    }
    let connections = state.connections();
    let (Some(sender), Some(creds)) = (&connections.mail, &connections.mail_credentials) else {
        return InviteDelivery::NoTransport;
    };
    let company_name = load_manifest(runtime)
        .await
        .ok()
        .flatten()
        .map(|m| m.company.name)
        .unwrap_or_else(|| runtime.id().as_ref().to_string());
    let base = state.config().host_base_url();
    let link = format!("{base}/login?company={}", runtime.id().as_ref());
    let days = INVITE_TTL_MILLIS / (24 * 60 * 60 * 1000);
    let mail = OutboundEmail {
        to: invite.email.clone(),
        subject: format!("You're invited to {company_name}"),
        body: format!(
            "{inviter} invited you to {company_name}.\n\n\
             To get in, sign in with this email address at:\n\n{link}\n\n\
             You'll be sent a sign-in link by email — this message isn't one, \
             and there's nothing in it to keep. The invitation is good for {days} days.\n\n\
             If you weren't expecting this, you can ignore it. Nothing has been \
             created for you and no one can act as you.\n"
        ),
    };
    match sender.send(creds, &mail).await {
        Ok(()) => InviteDelivery::Sent,
        Err(err) => {
            // The error, never the message: a body echoed into a log or into
            // telemetry carries the recipient's address off this host.
            // `error!`, not `warn!`: the default `EnvFilter` (no `RUST_LOG`)
            // shows errors only, and this is the only line that records why an
            // invite could not be delivered.
            tracing::error!(company = %runtime.id(), "invite mail failed: {err}");
            InviteDelivery::Failed
        }
    }
}

/// How to name the person who sent an invite, in mail the invitee reads.
///
/// A display name if they set one, otherwise one derived from the **local part**
/// of their address — never the full address. Same rule as chat attribution (see
/// `docs/spec/runtime/users.md`): being invited somewhere should not hand you
/// an admin's mailbox. Falls back to a role noun if the inviter cannot be
/// resolved, or has no name to derive — which is what a manifest- or
/// platform-bootstrapped id, and every wallet identity, looks like.
///
/// Through [`UserRecord::display_label`] rather than a local copy of the rule,
/// so an admin is called the same thing in this mail as on the console surfaces
/// the invitee will meet them on a minute later.
async fn inviter_label(runtime: &CompanyRuntime, user_id: &str) -> String {
    let found = runtime.users().get_user(runtime.id(), user_id).await.ok();
    found
        .flatten()
        .and_then(|user| user.display_label())
        .unwrap_or_else(|| "An admin".to_string())
}

/// `POST …/users/invites` — invite an address.
async fn invite(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<InviteBody>,
) -> Result<Json<InviteResult>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    // Refused before authenticating the caller is checked, because the answer
    // does not depend on who is asking: a company with no sign-in has no second
    // person to admit, and an invite would grant an account nobody could ever
    // reach. See `AuthMode::None`.
    if let Some(refusal) = wrong_mode_for_admin(&runtime) {
        return Err(refusal.into());
    }
    let admin = require_admin(&headers, &state, &runtime, peer).await?;
    let identity = body.identity(runtime.auth_mode())?;
    let now = now_millis();
    if runtime
        .users()
        .find_user_by_email(runtime.id(), &identity)
        .await?
        .is_some()
    {
        return Err(ApiError(OpenCompanyError::Conflict(format!(
            "{} is already a member",
            LoginIdentity::parse(&identity).label()
        )))
        .into_response()
        .into());
    }
    let mut record = InviteRecord {
        id: generate_id(),
        email: identity,
        role: body.role,
        invited_by: admin.user_id.clone(),
        created_at_millis: now,
        expires_at_millis: now + INVITE_TTL_MILLIS,
        accepted_at_millis: None,
        notified_at_millis: None,
    };
    // The store enforces one invite per address; a clash surfaces as 409.
    runtime.users().upsert_invite(runtime.id(), &record).await?;

    // Strictly after the grant lands. Mailing first would tell someone they
    // were invited by a request that then 409'd on a duplicate or failed in the
    // store — an invitation to a company that never invited them.
    let inviter = inviter_label(&runtime, &admin.user_id).await;
    let delivery = send_invite_mail(&state, &runtime, &record, &inviter).await;
    if delivery == InviteDelivery::Sent {
        let at = now_millis();
        // A stamp, not a re-write of the record: `record` was read before the
        // send and the send is a network round trip, so an admin who mistyped
        // the address has a real window to revoke this invite while the mail is
        // in flight. Writing the stale record back would restore the row they
        // just revoked — the address would be on the allowlist again with
        // nothing on screen to say so.
        match runtime
            .users()
            .mark_invite_notified(runtime.id(), &record.id, at)
            .await
        {
            Ok(true) => record.notified_at_millis = Some(at),
            // Revoked mid-flight. The revocation wins; the mail that already
            // left is a message about an invitation that no longer exists,
            // which is the lesser of the two wrongs available here.
            Ok(false) => {}
            // Best effort: the mail is already gone, so a failure to record
            // that must not fail the request. The roster row simply reads as
            // un-mailed, which understates rather than overstates what
            // happened.
            Err(err) => tracing::warn!(
                company = %runtime.id(),
                "invite mail sent but the record could not be stamped: {err}"
            ),
        }
    }
    Ok(Json(InviteResult {
        invite: record,
        delivery,
    }))
}

/// `DELETE …/users/invites/{invite_id}` — revoke an invite.
async fn revoke_invite(
    company: PublicCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    if let Some(refusal) = wrong_mode_for_admin(&runtime) {
        return Err(refusal.into());
    }
    require_admin(&headers, &state, &runtime, peer).await?;
    let invite_id = params.get("invite_id").cloned().unwrap_or_default();
    // A bootstrapped admin has no stored invite; revoking it would be a lie,
    // since its source would re-grant on the next login. The two sources are
    // withdrawn in different places, so say which one this row came from.
    if invite_id.starts_with("manifest:") {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "this admin comes from the company manifest; remove them from \
             [users].admins there instead"
                .to_string(),
        ))
        .into_response()
        .into());
    }
    if invite_id.starts_with("platform:") {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "this admin comes from the deployment's OPENCOMPANY_ADMIN_EMAIL; \
             unset it there instead"
                .to_string(),
        ))
        .into_response()
        .into());
    }
    let removed = runtime
        .users()
        .delete_invite(runtime.id(), &invite_id)
        .await?;
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
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<UpdateUser>,
) -> Result<Json<UserSummary>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    if let Some(refusal) = wrong_mode_for_admin(&runtime) {
        return Err(refusal.into());
    }
    require_admin(&headers, &state, &runtime, peer).await?;
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
        let name = name.trim().to_string();
        super::validate_display_name(&name)?;
        user.display_name = (!name.is_empty()).then_some(name);
    }
    user.updated_at_millis = now_millis();
    runtime.users().upsert_user(runtime.id(), &user).await?;

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
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<ResetPassword>,
) -> Result<Json<UserSummary>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    // A password is an alternative to a mailbox round trip, so it exists only
    // where the mailbox does. Issuing one in wallet mode would create a
    // credential no route accepts; in `none` mode there is nobody to issue it
    // to.
    if let Some(refusal) = crate::server::users::routes::wrong_mode_for_email(&runtime) {
        return Err(refusal.into());
    }
    require_admin(&headers, &state, &runtime, peer).await?;
    let user_id = params.get("user_id").cloned().unwrap_or_default();
    let mut user = load_user(&runtime, &user_id).await?;

    password::validate(&body.password, &user.email)?;
    let hash = password::hash(&token::OsTokens, &body.password)?;
    user.password_hash = Some(hash);
    user.must_change_password = true;
    user.updated_at_millis = now_millis();
    runtime.users().upsert_user(runtime.id(), &user).await?;

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
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, crate::server::Rejection> {
    let runtime = company.runtime.clone();
    // Nothing to revoke: a `none`-mode principal is resolved from configuration
    // on every request, so there is no session whose deletion would sign anyone
    // out. Succeeding would report a lever that does not exist.
    if let Some(refusal) = wrong_mode_for_admin(&runtime) {
        return Err(refusal.into());
    }
    require_admin(&headers, &state, &runtime, peer).await?;
    let user_id = params.get("user_id").cloned().unwrap_or_default();
    let user = load_user(&runtime, &user_id).await?;
    let revoked = runtime
        .sessions()
        .delete_for_user(runtime.id(), &user.id)
        .await?;
    Ok(Json(serde_json::json!({ "revoked": revoked })))
}

async fn load_user(
    runtime: &CompanyRuntime,
    user_id: &str,
) -> Result<UserRecord, crate::server::Rejection> {
    runtime
        .users()
        .get_user(runtime.id(), user_id)
        .await?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::InvalidRequest(format!(
                "no user {user_id}"
            )))
            .into_response()
            .into()
        })
}

/// Refuses to strip admin from the last active admin.
async fn ensure_not_last_admin(
    runtime: &CompanyRuntime,
    target: &UserRecord,
) -> Result<(), crate::server::Rejection> {
    if target.role != UserRole::Admin || target.status != UserStatus::Active {
        return Ok(());
    }
    let users = runtime.users().list_users(runtime.id()).await?;
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
        .into_response()
        .into());
    }
    Ok(())
}
