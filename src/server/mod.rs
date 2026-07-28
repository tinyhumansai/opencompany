#[cfg(feature = "tinyplace")]
pub mod a2a;
pub mod chat_history;
pub mod cors;
mod error;
pub mod feedback;
pub mod graphql;
pub mod hooks;
// Console MCP OAuth callback (issue #90): the unauthenticated browser-redirect
// landing route. Gated on `mcp` (it needs the OAuth token-exchange path).
#[cfg(feature = "mcp")]
pub mod mcp_oauth;
pub mod operator;
pub mod ops;
pub mod platform_auth;
pub mod provision;
mod routes;
pub mod users;

#[cfg(test)]
pub(crate) mod test_support;
pub mod webhook;

pub use error::ApiError;
pub use routes::{router, serve};
