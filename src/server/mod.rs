mod error;
pub mod operator;
mod routes;

pub use error::ApiError;
pub use routes::{router, serve};
