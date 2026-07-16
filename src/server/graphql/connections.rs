//! Connection / domain / SMTP status reads over the [`SecretStore`] reserved
//! keys. Every value here is a **non-secret projection**: the OAuth token
//! material and the SMTP password never appear in a response.

use std::sync::Arc;

use async_graphql::SimpleObject;

use crate::company::dns::DomainStatus;
use crate::company::runtime::CompanyRuntime;
use crate::server::ops::smtp::{SmtpCredentials, SmtpStatus};

/// The reserved SecretStore key holding the JSON domain status.
const DOMAIN_KEY: &str = "__domain";
/// The reserved SecretStore key holding the JSON SMTP credentials.
const SMTP_KEY: &str = "__smtp";

/// One third-party connection's state: manifest intent plus live OAuth status.
#[derive(SimpleObject)]
#[graphql(name = "ConnectionState")]
pub struct ConnectionStateGql {
    /// The provider id (e.g. `slack`, `gmail`, `github`).
    pub provider: String,
    /// Whether an OAuth token is stored for this provider.
    pub connected: bool,
    /// The connected account label, when known.
    pub account: Option<String>,
    /// The manifest's stated reason for wanting this connection.
    pub reason: Option<String>,
}

/// A generated DNS record for custom-domain verification. Mirrors
/// [`DnsRecord`](crate::company::dns::DnsRecord).
#[derive(SimpleObject)]
#[graphql(name = "DnsRecord")]
pub struct DnsRecordGql {
    /// The record type (`CNAME` | `TXT`).
    #[graphql(name = "type")]
    pub record_type: String,
    /// The record name/host.
    pub name: String,
    /// The record value.
    pub value: String,
    /// The record TTL.
    pub ttl: String,
}

/// Custom-domain status. Mirrors [`DomainStatus`].
#[derive(SimpleObject)]
#[graphql(name = "DomainStatus")]
pub struct DomainStatusGql {
    /// The configured domain.
    pub domain: String,
    /// Whether the domain's records have been verified.
    pub verified: bool,
    /// The DNS records the operator must publish.
    pub records: Vec<DnsRecordGql>,
}

impl From<DomainStatus> for DomainStatusGql {
    fn from(status: DomainStatus) -> Self {
        Self {
            domain: status.domain,
            verified: status.verified,
            records: status
                .records
                .into_iter()
                .map(|record| DnsRecordGql {
                    record_type: record.record_type,
                    name: record.name,
                    value: record.value,
                    ttl: record.ttl,
                })
                .collect(),
        }
    }
}

/// Non-secret SMTP status: host/port/username only — never the password.
#[derive(SimpleObject)]
#[graphql(name = "SmtpStatus")]
pub struct SmtpStatusGql {
    /// The SMTP host (empty when unconfigured).
    pub host: String,
    /// The SMTP port (0 when unconfigured).
    pub port: i32,
    /// The SMTP username (empty when unconfigured).
    pub username: String,
    /// Whether SMTP is configured.
    pub configured: bool,
}

impl From<SmtpStatus> for SmtpStatusGql {
    fn from(status: SmtpStatus) -> Self {
        Self {
            host: status.host.unwrap_or_default(),
            port: status.port.map(i32::from).unwrap_or(0),
            username: status.username.unwrap_or_default(),
            configured: status.configured,
        }
    }
}

/// Resolves `Company.connections`: manifest intent merged with OAuth status.
pub(crate) async fn resolve_connections(
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<Vec<ConnectionStateGql>> {
    let Some(record) = runtime.store().load(runtime.id()).await? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(record.manifest.connections.len());
    for connection in &record.manifest.connections {
        let key = format!("oauth/{}", connection.provider);
        let (connected, account) = match runtime.secrets().get(runtime.id(), &key).await? {
            Some(value) if !value.expose().trim().is_empty() => {
                let account = serde_json::from_str::<serde_json::Value>(value.expose())
                    .ok()
                    .and_then(|json| {
                        json.get("account")
                            .and_then(|a| a.as_str())
                            .map(str::to_string)
                    });
                (true, account)
            }
            _ => (false, None),
        };
        out.push(ConnectionStateGql {
            provider: connection.provider.clone(),
            connected,
            account,
            reason: connection.reason.clone(),
        });
    }
    Ok(out)
}

/// Resolves `Company.domain`, returning null when no domain is configured.
pub(crate) async fn resolve_domain(
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<Option<DomainStatusGql>> {
    let Some(value) = runtime.secrets().get(runtime.id(), DOMAIN_KEY).await? else {
        return Ok(None);
    };
    let status: DomainStatus = serde_json::from_str(value.expose())
        .map_err(|e| async_graphql::Error::new(format!("stored domain status is invalid: {e}")))?;
    Ok(Some(status.into()))
}

/// Resolves `Company.smtp`: the non-secret SMTP projection (never the password).
pub(crate) async fn resolve_smtp(
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<SmtpStatusGql> {
    let status = match runtime.secrets().get(runtime.id(), SMTP_KEY).await? {
        Some(value) => {
            let creds: SmtpCredentials = serde_json::from_str(value.expose()).map_err(|e| {
                async_graphql::Error::new(format!("stored SMTP credentials are invalid: {e}"))
            })?;
            SmtpStatus::from_credentials(&creds)
        }
        None => SmtpStatus::unconfigured(),
    };
    Ok(status.into())
}
