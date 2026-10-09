//! Typed failures for the directory routes.
//!
//! The upstream registry is slow and sometimes silent, and its errors carry
//! transport detail and upstream addresses. The console gets a stable code and
//! an operator-facing sentence instead; the raw cause goes to the log only.

use std::future::Future;
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// How long a directory search may take before the route answers
/// `registry_timeout`.
pub(in crate::server::ops) const SEARCH_BUDGET: Duration = Duration::from_secs(8);

/// How long one featured-connector lookup may take before the first page is
/// served without it.
pub(in crate::server::ops) const FEATURED_LOOKUP_BUDGET: Duration = Duration::from_secs(4);

/// Why a directory read did not produce an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server::ops) enum RegistryFailure {
    /// The registry did not answer within the budget.
    Timeout,
    /// The registry answered with an error or could not be reached.
    Unavailable,
}

/// Which directory read failed, so the sentence can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server::ops) enum RegistryRead {
    Search,
    Lookup,
}

impl RegistryFailure {
    /// Classifies an upstream error by its message. Only the class leaves the
    /// host; the message itself is logged.
    pub(in crate::server::ops) fn classify(message: &str) -> Self {
        let lower = message.to_ascii_lowercase();
        if lower.contains("timed out") || lower.contains("timeout") || lower.contains("deadline") {
            Self::Timeout
        } else {
            Self::Unavailable
        }
    }

    /// The stable wire code a client branches on.
    pub(in crate::server::ops) fn code(self) -> &'static str {
        match self {
            Self::Timeout => "registry_timeout",
            Self::Unavailable => "registry_unavailable",
        }
    }

    /// The HTTP status: a gateway timeout or a service outage.
    pub(in crate::server::ops) fn status(self) -> StatusCode {
        match self {
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// The operator-facing sentence. Names no upstream address or cause.
    pub(in crate::server::ops) fn message(self, read: RegistryRead) -> &'static str {
        match (self, read) {
            (Self::Timeout, RegistryRead::Search) => {
                "The MCP directory is taking too long to search right now. Try again in a moment."
            }
            (Self::Timeout, RegistryRead::Lookup) => {
                "The MCP directory is taking too long to answer for this server right now. Try \
                 again in a moment."
            }
            (Self::Unavailable, RegistryRead::Search) => {
                "The MCP directory can't be searched right now. Try again in a moment."
            }
            (Self::Unavailable, RegistryRead::Lookup) => {
                "The MCP directory can't look up this server right now. Try again in a moment."
            }
        }
    }

    /// The `{ error, code }` envelope every other API error uses.
    pub(in crate::server::ops) fn response(self, read: RegistryRead) -> Response {
        (
            self.status(),
            Json(json!({ "error": self.message(read), "code": self.code() })),
        )
            .into_response()
    }
}

/// Runs one directory read under `budget`, mapping a late or failed answer to a
/// [`RegistryFailure`] and logging the raw cause.
pub(in crate::server::ops) async fn bounded<T, E, Fut>(
    read: RegistryRead,
    budget: Option<Duration>,
    work: Fut,
) -> Result<T, RegistryFailure>
where
    E: std::fmt::Display,
    Fut: Future<Output = Result<T, E>>,
{
    let outcome = match budget {
        Some(budget) => match tokio::time::timeout(budget, work).await {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::warn!("[mcp-registry] {read:?} exceeded its {budget:?} budget");
                return Err(RegistryFailure::Timeout);
            }
        },
        None => work.await,
    };
    outcome.map_err(|error| {
        let failure = RegistryFailure::classify(&error.to_string());
        tracing::warn!(
            "[mcp-registry] {read:?} failed ({}): {error}",
            failure.code()
        );
        failure
    })
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod tests;
