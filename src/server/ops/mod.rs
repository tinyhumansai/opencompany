//! The `ops` write-plane router family: connections (OAuth), custom
//! domain/DNS, SMTP credentials + test send, and the inbound email ingest
//! transport.
//!
//! Every credential-shaped value written here lands in
//! [`SecretStore`](crate::ports::SecretStore); the responses expose only
//! non-secret status (connected / verified / configured). Routes follow the
//! same dual scoping as the operator surface: the platform `{id}` form and the
//! prosumer single-company alias (`/api/v1/company/â¦`).
//!
//! Everything that touches the network â real DNS lookups (`dns`), real SMTP
//! send (`smtp`), and real OAuth token exchange (`oauth`) â is dependency-
//! inverted behind a trait so the default build stays offline. The
//! [`ConnectionsRuntime`] seam carries the injected resolver/sender (offline
//! mocks in tests, real impls when a feature is on); the OAuth write routes are
//! compiled only under the `oauth` feature and 404 otherwise.

/// `GET {scope}/activation` — the account-activation funnel read projection
/// (issue #1843). See [`crate::company::activation`] for the derivation.
pub mod activation;
pub mod artifacts;
/// Avatar uploads (`docs/spec/runtime/avatars.md`): the custom-image half of
/// choosing a face for a teammate or for yourself.
pub mod avatars;
pub mod billing;
/// `GET {scope}/agents/{agent_id}/budget-pause` and
/// `POST {scope}/agents/{agent_id}/budget-pause/redeem` (issue #1846): read
/// and redeem the durable re-issue marker a top-level turn parks when it
/// pauses for lack of inference budget/credits. The console's Add-Credits CTA.
pub mod budget_pause;
pub mod capabilities;
pub mod company_key;
/// Operator-set company logo, stored in the company manifest.
pub mod company_logo;
/// `PATCH {scope}` — the account-activation funnel's conscious naming step
/// (issue #1844): sets the company's display name and stamps
/// [`crate::ports::types::CompanyRecord::name_confirmed`]. See
/// [`crate::company::activation`] for how that flag feeds the funnel.
pub mod company_profile;
pub mod composio;
pub mod composio_toolkits;
pub mod connections_read;
pub mod deep_trace;
pub mod domain;
pub mod finance;
pub mod finances;
/// `GET {scope}/harnesses` (issue #1245's harness-picker follow-up): the
/// company's declared `[[harness]]` set, read-only. What Settings' Harnesses
/// page and the per-agent Harness picker both read.
pub mod harnesses;
pub mod hosting;
pub mod imap;
pub mod inbox;
pub mod inference;
pub mod language;
/// The dynamic-ledger surface: list, declare, read, record, delete, retire.
/// Reads and writes route through [`crate::company::ledgers`], which is where
/// the one deletion rule lives.
pub mod ledgers;
pub mod mail;
pub mod mailer;
pub mod mcp;
pub mod mcp_config;
pub mod mcp_registry;
pub mod memory;
pub mod memory_engine;
pub mod memory_ingest;
/// The `@` picker's directory: every teammate, person, desk and broadcast token
/// a mention can name, in one member-safe read. See [`mentions`].
pub mod mentions;
/// The notification feed, and the mention badge it backs. The first consumer
/// of `NotificationStore`. See [`notifications`].
pub mod notifications;
pub mod pages;
pub mod policy;
/// Who is here and who is typing: heartbeat, clean disconnect, typing ping.
/// Ephemeral, leased, never journaled. See [`presence`].
pub mod presence;
pub mod read_state;
pub mod runs;
pub mod scope;
/// Per-company web search settings: which provider the agents search through,
/// and the write-only key behind it. See [`crate::company::search`].
pub mod search;
/// First-run company setup: propose a starting roster from three answers
/// (`docs/spec/runtime/company-setup.md`). Proposes only — the console creates
/// each teammate through [`team`], so setup has no second write path.
pub mod setup;
pub mod skills;
pub mod smtp;
mod task_cost;
pub mod task_export;
pub mod tasks;
pub mod team;
/// Issue #264: one agent, opened â `GET`/`PATCH {scope}/team/{agent_id}`. The
/// detail read (identity, tier, **resolved** tool grants, desks) and the edit
/// for the fields the console owns. Attached to [`team`]'s existing
/// `/team/{agent_id}` route rather than merged as its own. See [`team_agent`].
mod team_agent;
/// The unified tool catalog read (`GET {scope}/tools/catalog`): everything this
/// company can grant an agent — built-ins, MCP servers and Composio toolkits —
/// in one vocabulary. Read-only and openhuman-free.
pub mod tool_catalog;
pub mod tool_grants;
pub mod usage;
pub mod workflows;
pub mod workspace;

#[cfg(feature = "oauth")]
pub mod connections;

pub(crate) use scope::{AdminScopedCompany, ScopedCompany, scoped};

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
/// SecretStore key holding the JSON SMTP configuration — everything but the
/// password. See [`SMTP_PASSWORD_KEY`].
pub(crate) const SMTP_KEY: &str = "__smtp";
/// SecretStore key holding the SMTP password on its own.
///
/// Split out of [`SMTP_KEY`] so that "keep the stored password" is the
/// *absence* of a write rather than a read-modify-write: `PUT …/smtp` without a
/// password rewrites only the configuration blob and never touches this key, so
/// it cannot write a stale secret back over a concurrent rotation. Credentials
/// written before the split still carry the password inside the `SMTP_KEY`
/// blob; reads fall back to it, and the first write after the split migrates it
/// here. See `src/server/ops/smtp.rs`.
pub(crate) const SMTP_PASSWORD_KEY: &str = "__smtp_password";
/// SecretStore key holding the shared secret the inbound ingest HMAC is verified against.
pub(crate) const INGEST_SECRET_KEY: &str = "ingest_secret";

/// The injected network seams for the credential surfaces. Empty by default
/// (the offline build), populated by the `serve` entrypoint (real impls under
/// their features) or by tests (offline mocks).
#[derive(Clone, Default)]
pub struct ConnectionsRuntime {
    /// Resolver used by `POST â¦/domain/verify`. When `None`, verify is
    /// "not wired yet" (404).
    pub dns: Option<Arc<dyn DnsResolver>>,
    /// Sender used by `POST â¦/smtp/test` and outbound mail. When `None`,
    /// test-send is "not wired yet" (404).
    pub mail: Option<Arc<dyn MailSender>>,
    /// Host-level outbound credentials (`OPENCOMPANY_MAIL_*`), used for
    /// platform mail such as login links â mail sent on the platform's behalf
    /// rather than a company's. A company's own outbound instead reads its
    /// `SecretStore`, so a tenant never sees this credential. `None` means the
    /// host sends no platform mail.
    pub mail_credentials: Option<MailCredentials>,
}

impl ConnectionsRuntime {
    /// An empty runtime â every networked surface degrades to "not wired".
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
        .merge(capabilities::router())
        .merge(harnesses::router())
        .merge(budget_pause::router())
        .merge(tool_catalog::router())
        .merge(connections_read::router())
        .merge(billing::router())
        .merge(hosting::router())
        .merge(search::router())
        .merge(company_logo::router())
        .merge(company_key::router())
        .merge(company_profile::router())
        .merge(composio::router())
        .merge(domain::router())
        .merge(deep_trace::router())
        .merge(finance::router())
        .merge(finances::router())
        .merge(usage::router())
        .merge(smtp::router())
        .merge(inbox::router())
        .merge(tasks::router())
        .merge(ledgers::router())
        .merge(task_export::router())
        .merge(runs::router())
        .merge(artifacts::router())
        .merge(avatars::router())
        .merge(memory::router())
        .merge(memory_engine::router())
        .merge(memory_ingest::router())
        .merge(workspace::router())
        .merge(pages::router())
        .merge(skills::router())
        .merge(mcp::router())
        .merge(mcp_config::router())
        .merge(mcp_registry::router())
        .merge(read_state::router())
        .merge(notifications::router())
        .merge(presence::router())
        .merge(mentions::router())
        .merge(inference::router())
        .merge(team::router())
        .merge(setup::router())
        .merge(activation::router())
        .merge(policy::router())
        .merge(tool_grants::router())
        .merge(workflows::router())
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

/// A `409 restart_required` response for a surface that this build *does* have,
/// but this company's running runtime does not â because the runtime was wired
/// at boot, before the inference config that would have wired it existed
/// (issue #266).
///
/// Deliberately not the `not_wired` 404: the console's bare-catch treats that as
/// a permanent capability gap and degrades to a read-only view, which is the
/// wrong answer for a state one restart clears. A distinct `code` lets the
/// console say what to do instead of hiding the button.
pub(crate) fn restart_required(what: &str) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    (
        StatusCode::CONFLICT,
        axum::Json(serde_json::json!({
            "error": format!(
                "{what} is not wired on this company's running runtime: it started with no \
                 inference source. Restart the company to pick up the saved configuration."
            ),
            "code": "restart_required",
        })),
    )
        .into_response()
}

/// A `409 inference_required` response for a workflow-run attempt on a company
/// that has *never* configured an inference source â `workflow_runner()` is
/// `None` because there is no brain to run yet, not because this deployment
/// lacks execution and not because a saved config is stranded behind a boot
/// decision (issue #514).
///
/// Deliberately neither of the sibling responses:
///   - not the `not_wired` 404: the console's bare-catch treats that as a
///     permanent capability gap and degrades to a read-only view â the wrong
///     answer for a state the operator clears by configuring a provider (which
///     triggers issue #290's rebuild-in-place; no restart needed);
///   - not `restart_required`: nothing was ever saved to the strand, so there is
///     no configuration for a restart to pick up.
///
/// The `code` is a contract the console keys its own copy on; the `error` prose
/// is the fallback a curl user sees. It never says "deployment" or "not wired" â
/// the deployment is fine.
pub(crate) fn inference_required(what: &str) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    (
        StatusCode::CONFLICT,
        axum::Json(serde_json::json!({
            "error": format!(
                "{what} needs an inference source, and none is configured for this \
                 company. Set a provider in Settings â Inference, then run again."
            ),
            "code": "inference_required",
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
