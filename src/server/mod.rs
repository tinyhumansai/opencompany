#[cfg(feature = "tinyplace")]
pub mod a2a;
mod error;
pub mod feedback;
pub mod graphql;
pub mod operator;
pub mod ops;
pub mod platform_auth;
pub mod provision;
mod routes;
pub mod webhook;

pub use error::ApiError;
pub use routes::{router, serve};
