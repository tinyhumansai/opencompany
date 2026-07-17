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
//! - [`token`]: minting and hashing the two secrets.

pub mod token;
