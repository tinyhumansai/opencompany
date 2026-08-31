#[cfg(feature = "tinyplace")]
pub mod a2a;
/// The Agent Client Protocol surface (the `acp` feature).
///
/// The module's own docs reason about "a build without the feature"; this is
/// that feature. `routes::RESERVED_PREFIXES` keeps `/acp` 404ing either way, so
/// a client probing a host without it gets an honest answer rather than the
/// console shell.
#[cfg(feature = "acp")]
pub mod acp;
pub(crate) mod approval_visibility;
pub mod chat_history;
pub mod cognition;
pub mod cors;
mod error;
pub mod feedback;
pub mod feedback_board;
pub mod graphql;
pub mod hooks_chargebee;
pub mod hub_identity;
pub(crate) mod inference_models;
// Console MCP OAuth callback (issue #90): the unauthenticated browser-redirect
// landing route. Gated on `mcp` (it needs the OAuth token-exchange path).
#[cfg(feature = "mcp")]
pub mod mcp_oauth;
pub mod operator;
pub mod ops;
pub mod platform_auth;
/// Who is here, and who is typing — ephemeral, leased, and never journaled.
/// See [`presence`].
pub mod presence;
pub mod provision;
mod routes;
/// The first-run setup flow: one surface that configures an instance.
pub mod setup;
pub mod shutdown;
pub mod users;

#[cfg(test)]
pub(crate) mod test_support;
pub mod webhook;

pub use error::{ApiError, Rejection};
pub use routes::{Serving, bind, router, serve, serve_on, serve_on_until};
