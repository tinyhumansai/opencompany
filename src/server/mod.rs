#[cfg(feature = "tinyplace")]
pub mod a2a;
pub mod cors;
mod error;
pub mod feedback;
pub mod hooks;
pub mod graphql;
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
