//! The `ops` write-plane router family: connections (OAuth), custom
//! domain/DNS, SMTP credentials + test send, and the inbound email ingest
//! transport.
//!
//! Every credential-shaped value written here lands in
//! [`SecretStore`](crate::ports::SecretStore); the responses expose only
//! non-secret status (connected / verified / configured). Routes follow the
//! same dual scoping as the operator surface: the platform `{id}` form and the
//! prosumer single-company alias (`/api/v1/company/…`).
//!
//! Everything that touches the network — real DNS lookups (`dns`), real SMTP
//! send (`smtp`), and real OAuth token exchange (`oauth`) — is dependency-
//! inverted behind a trait so the default build stays offline. The
//! [`ConnectionsRuntime`] seam carries the injected resolver/sender (offline
//! mocks in tests, real impls when a feature is on); the OAuth write routes are
//! compiled only under the `oauth` feature and 404 otherwise.

pub mod domain;
pub mod inbox;
pub mod language;
pub mod mail;
pub mod mailer;
pub mod memory;
pub mod scope;
pub mod skills;
pub mod smtp;
pub mod tasks;
pub mod team;
pub mod workspace;

#[cfg(feature = "oauth")]
pub mod connections;

pub(crate) use scope::{ScopedCompany, scoped};

#[cfg(test)]
mod test;
#[cfg(test)]
mod write_test;

use std::sync::Arc;

use axum::Router;

use crate::AppState;
use crate::company::dns::DnsResolver;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;
use crate::server::error::ApiError;
use crate::server::ops::mailer::{MailCredentials, MailSender};

/// SecretStore key holding the JSON [`DomainStatus`](crate::company::dns::DomainStatus).
pub(crate) const DOMAIN_KEY: &str = "__domain";
/// SecretStore key holding the JSON SMTP credentials.
pub(crate) const SMTP_KEY: &str = "__smtp";
/// SecretStore key holding the shared secret the inbound ingest HMAC is verified against.
pub(crate) const INGEST_SECRET_KEY: &str = "ingest_secret";

/// The injected network seams for the credential surfaces. Empty by default
/// (the offline build), populated by the `serve` entrypoint (real impls under
/// their features) or by tests (offline mocks).
#[derive(Clone, Default)]
pub struct ConnectionsRuntime {
    /// Resolver used by `POST …/domain/verify`. When `None`, verify is
    /// "not wired yet" (404).
    pub dns: Option<Arc<dyn DnsResolver>>,
    /// Sender used by `POST …/smtp/test` and outbound mail. When `None`,
    /// test-send is "not wired yet" (404).
    pub mail: Option<Arc<dyn MailSender>>,
    /// Host-level outbound credentials (`OPENCOMPANY_MAIL_*`), used for
    /// platform mail such as login links — mail sent on the platform's behalf
    /// rather than a company's. A company's own outbound instead reads its
    /// `SecretStore`, so a tenant never sees this credential. `None` means the
    /// host sends no platform mail.
    pub mail_credentials: Option<MailCredentials>,
}

impl ConnectionsRuntime {
    /// An empty runtime — every networked surface degrades to "not wired".
    pub fn new() -> Self {
        Self::default()
    }

    /// Injects a DNS resolver (real under `dns`, a mock in tests).
    pub fn with_dns(mut self, dns: Arc<dyn DnsResolver>) -> Self {
        self.dns = Some(dns);
        self
    }

    /// Injects a mail sender (real under `smtp`, a mock in tests).
    pub fn with_mail(mut self, mail: Arc<dyn MailSender>) -> Self {
        self.mail = Some(mail);
        self
    }

    /// Injects the host-level credentials platform mail is sent with.
    pub fn with_mail_credentials(mut self, creds: MailCredentials) -> Self {
        self.mail_credentials = Some(creds);
        self
    }
}

impl std::fmt::Debug for ConnectionsRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `MailCredentials`' own Debug redacts the password.
        f.debug_struct("ConnectionsRuntime")
            .field("dns", &self.dns.is_some())
            .field("mail", &self.mail.is_some())
            .field("mail_credentials", &self.mail_credentials)
            .finish()
    }
}

/// Builds the `ops` route fragment, merged into the main router.
pub fn router() -> Router<AppState> {
    let router = Router::new()
        .merge(domain::router())
        .merge(smtp::router())
        .merge(inbox::router())
        .merge(tasks::router())
        .merge(memory::router())
        .merge(workspace::router())
        .merge(skills::router())
        .merge(team::router())
        .merge(mail::router());
    #[cfg(feature = "oauth")]
    let router = router.merge(connections::router());
    router
}

/// The SecretStore key holding a provider's stored OAuth tokens.
#[cfg(feature = "oauth")]
pub(crate) fn oauth_key(provider: &str) -> String {
    format!("oauth/{provider}")
}

/// Resolves a company runtime by id.
pub(crate) fn resolve(state: &AppState, id: &str) -> Result<Arc<CompanyRuntime>, ApiError> {
    state
        .registry()
        .get(&CompanyId::new(id))
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(id.to_string())))
}

/// A `404 not_wired` response for a surface whose networked seam is absent in
/// this build (no `dns`/`smtp` resolver injected). The console's bare-catch
/// treats it as "not wired yet" and falls back to the read-only view.
pub(crate) fn not_wired(what: &str) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": format!("{what} is not wired in this deployment"),
            "code": "not_wired",
        })),
    )
        .into_response()
}

/// Resolves the sole registered company (single-company alias).
pub(crate) fn resolve_sole(state: &AppState) -> Result<Arc<CompanyRuntime>, ApiError> {
    state.registry().sole().ok_or_else(|| {
        ApiError(OpenCompanyError::CompanyNotFound(
            "single-company".to_string(),
        ))
    })
}
