//! Shared test helpers for driving authenticated requests.
//!
//! Every route now requires a principal — there is no dev mode that lets an
//! anonymous request through. Tests therefore have to present one, and this is
//! the one place that mints it, so the shape of "an authenticated request" is
//! stated once rather than in every test module.

#![cfg(test)]

use crate::AppState;
use crate::ports::types::CompanyId;
use crate::ports::{SessionRecord, UserRecord, UserRole, UserStatus, generate_id, now_millis};
use crate::server::users::cookie::session_cookie_name;
use crate::server::users::token::{OsTokens, mint_session_token, sha256_hex};

/// Seeds an active user with a live session in `company` and returns the
/// `Cookie` header value a browser would send.
///
/// The session is minted through the real stores and the real token hashing, so
/// tests exercise the same resolution path production does — the only shortcut
/// is skipping the magic-link round trip.
pub(crate) async fn seed_session(state: &AppState, company: &str, role: UserRole) -> String {
    let id = CompanyId::new(company);
    let runtime = state
        .registry()
        .get(&id)
        .expect("seed_session: company is not registered");
    let now = now_millis();
    let user_id = generate_id();
    runtime
        .users()
        .upsert_user(
            &id,
            &UserRecord {
                id: user_id.clone(),
                email: format!("{}@example.test", &user_id[..8]),
                display_name: None,
                role,
                status: UserStatus::Active,
                password_hash: None,
                must_change_password: false,
                created_at_millis: now,
                last_seen_at_millis: None,
                updated_at_millis: now,
            },
        )
        .await
        .expect("seed_session: upsert user");

    let token = mint_session_token(&OsTokens);
    runtime
        .sessions()
        .create(
            &id,
            &SessionRecord {
                id: generate_id(),
                token_hash: sha256_hex(&token),
                user_id,
                created_at_millis: now,
                expires_at_millis: now + 60 * 60 * 1000,
                last_seen_at_millis: now,
                user_agent: None,
            },
        )
        .await
        .expect("seed_session: create session");

    format!(
        "{}={token}",
        session_cookie_name(&id).expect("seed_session: company id cannot name a cookie")
    )
}

/// Seeds an admin session — the common case for tests that drive write routes.
pub(crate) async fn seed_admin(state: &AppState, company: &str) -> String {
    seed_session(state, company, UserRole::Admin).await
}

/// A fixed session token for the harnesses whose request helpers do not thread
/// a cookie through every call site.
///
/// Test-only, and safe precisely because it is: only its *hash* is stored, so
/// this constant grants nothing except against a store a test just seeded.
pub(crate) const FIXED_TEST_TOKEN: &str = "fixed-test-session-token-not-a-secret";

/// Seeds an admin whose session uses [`FIXED_TEST_TOKEN`].
///
/// Lets a harness attach [`fixed_cookie`] to every request without rewriting
/// each test to carry a cookie it does not care about.
pub(crate) async fn seed_fixed_admin(state: &AppState, company: &str) {
    let id = CompanyId::new(company);
    let runtime = state
        .registry()
        .get(&id)
        .expect("seed_fixed_admin: company is not registered");
    let now = now_millis();
    let user_id = generate_id();
    runtime
        .users()
        .upsert_user(
            &id,
            &UserRecord {
                id: user_id.clone(),
                email: "harness-admin@example.test".to_string(),
                display_name: None,
                role: UserRole::Admin,
                status: UserStatus::Active,
                password_hash: None,
                must_change_password: false,
                created_at_millis: now,
                last_seen_at_millis: None,
                updated_at_millis: now,
            },
        )
        .await
        .expect("seed_fixed_admin: upsert user");
    runtime
        .sessions()
        .create(
            &id,
            &SessionRecord {
                id: generate_id(),
                token_hash: sha256_hex(FIXED_TEST_TOKEN),
                user_id,
                created_at_millis: now,
                expires_at_millis: now + 60 * 60 * 1000,
                last_seen_at_millis: now,
                user_agent: None,
            },
        )
        .await
        .expect("seed_fixed_admin: create session");
}

/// The `Cookie` header for [`seed_fixed_admin`]'s session in `company`.
pub(crate) fn fixed_cookie(company: &str) -> String {
    format!(
        "{}={FIXED_TEST_TOKEN}",
        session_cookie_name(&CompanyId::new(company)).expect("cookie-safe company id")
    )
}
