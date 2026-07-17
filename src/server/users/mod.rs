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
//! - [`cookie`]: naming, parsing, and rendering the session cookie.
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
pub mod cookie;
pub mod password;
pub mod routes;
pub mod token;

pub(crate) mod scope;

pub use routes::router;

#[cfg(test)]
mod auth_test;
#[cfg(test)]
mod routes_test;
