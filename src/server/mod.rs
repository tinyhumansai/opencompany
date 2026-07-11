#[cfg(feature = "tinyplace")]
pub mod a2a;
mod error;
pub mod feedback;
pub mod operator;
pub mod platform_auth;
pub mod provision;
mod routes;
pub mod webhook;

pub use error::ApiError;
pub use routes::{router, serve};
