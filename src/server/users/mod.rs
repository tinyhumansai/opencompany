//! Human user authentication: magic-link login, sessions, and invites.
//!
//! This is the surface humans use to reach a company. It is separate from — and
//! deliberately weaker than — the machine credentials in
//! [`platform_auth`](crate::server::platform_auth): a user is a collaborator
//! inside one company, never an operator.
//!
//! The flow, end to end:
//!
//! 1. An admin invites an address, or the company manifest names it an admin.
//! 2. `POST …/auth/request` mints a [`token`] login code, stores its hash, and
//!    mails a link. An uninvited address gets the same response and no mail.
//! 3. `POST …/auth/verify` redeems the code exactly once (enforced in the
//!    store), mints a session, and sets an `HttpOnly` cookie.
//! 4. Every later request resolves that cookie to a user scoped to one company.
//!
//! Sub-modules:
//!
//! - [`token`]: minting and hashing the login/session secrets.
//! - [`password`]: optional password hashing, verification, and policy.
//! - [`cookie`]: naming, parsing, and rendering the session carriers.
//! - [`wallet`]: signing a challenge with an Ed25519 key instead of holding a
//!   mailbox, for a company whose
//!   [`AuthMode`](crate::app::config::AuthMode) is `wallet`.
//!
//! ## Two carriers, one session
//!
//! Step 4 above says "cookie", and for a browser it is. A desktop client cannot
//! use one: `SameSite=Lax` means the browser never sends a cookie cross-site,
//! and such a client is cross-site with every server it talks to. It presents
//! the same session in [`cookie::SESSION_HEADER`] instead.
//!
//! A *paired device* is what that client holds — the same [`SessionRecord`]
//! with [`SessionKind::Device`], a label, and a year-long TTL, minted by
//! a code a signed-in human pasted in. Deliberately not a
//! separate credential system, so that suspension, admin reset and a user's own
//! password change already revoke sessions without any of them growing a second
//! code path to remember.
//!
//! [`SessionRecord`]: crate::ports::SessionRecord
//! [`SessionKind::Device`]: crate::ports::SessionKind::Device
//!
//! ## Passwords are optional
//!
//! A user may set a password to skip the round trip through their mailbox, but
//! the magic link always works and a user who never sets one is unaffected.
//! There is deliberately **no separate password-reset credential**: "forgot my
//! password" is a magic-link login followed by setting a new one, which reuses
//! the path above rather than adding a second emailed secret to get wrong.
//!
//! An admin may instead set a *temporary* password, which revokes the user's
//! sessions and flags the account so the user is asked to replace it.

pub mod admin;
pub mod bootstrap;
pub mod cookie;
pub mod password;
pub mod routes;
pub mod token;
pub mod wallet;

pub(crate) mod scope;

pub use routes::router;

use crate::error::OpenCompanyError;

/// The longest a display name may be, in characters.
///
/// A name renders on every surface that shows a person — chat gutters,
/// facepiles, the org chart, approvals — and rides in every roster payload,
/// so the bound is the difference between a name and a place to park a page of
/// text. Eighty is far above any real name and far below anything a UI should
/// be asked to lay out.
pub(crate) const MAX_DISPLAY_NAME_CHARS: usize = 80;

/// Rejects a display name longer than [`MAX_DISPLAY_NAME_CHARS`], as `400`.
pub(crate) fn validate_display_name(name: &str) -> Result<(), OpenCompanyError> {
    if name.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a display name may be at most {MAX_DISPLAY_NAME_CHARS} characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod auth_test;
#[cfg(test)]
mod hub_test;
#[cfg(test)]
mod mode_test;
#[cfg(test)]
mod routes_test;
