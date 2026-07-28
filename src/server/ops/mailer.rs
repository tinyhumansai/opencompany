//! The provider-agnostic outbound mail adapter.
//!
//! Sending mail has two independent axes, and this module separates them:
//!
//! - **What to send** — [`OutboundEmail`], the same for every provider.
//! - **How to send it** — [`MailCredentials`], a provider-tagged enum. The tag
//!   is what makes the adapter pluggable: a stored or configured credential
//!   blob names its own transport, so nothing has to be told out-of-band which
//!   provider it belongs to.
//!
//! [`MailSender`] is the seam. It takes `MailCredentials` rather than any one
//! provider's credential type, so adding AWS SES, Resend, Postmark, or anything
//! else means adding a variant plus a sender — and because the variant makes
//! every existing `match` non-exhaustive, the compiler names each place that
//! has to account for it. That is the point of the enum over a `Box<dyn Any>`
//! style config.
//!
//! ## Adding a provider
//!
//! 1. Add a credentials struct and a [`MailCredentials`] variant for it.
//! 2. Add a [`MailProvider`] variant and map it in [`MailCredentials::provider`].
//! 3. Implement [`MailSender`] for it in its own module, behind its own feature
//!    so the default build keeps linking no network crates.
//! 4. Teach [`MailConfig::from_env`] to resolve it.
//!
//! ## Two credential scopes
//!
//! - **Host-level** ([`MailConfig::from_env`], `OPENCOMPANY_MAIL_*`): one
//!   provider for the whole host. This is what platform mail — login links —
//!   uses, because a login link is sent on the platform's behalf, not the
//!   company's.
//! - **Per-company** (the company's `SecretStore` under `__smtp`): a company's
//!   own outbound identity, used by the test-send route and per-teammate mail.
//!
//! Both flow through the same [`MailSender`]; they differ only in where the
//! credentials come from.
//!
//! ## Note on AWS SES
//!
//! SES exposes an SMTP submission endpoint, so SES already works through
//! [`MailProvider::Smtp`] by pointing the host at
//! `email-smtp.<region>.amazonaws.com` with SES SMTP credentials. A native
//! `Ses` variant would only be worth adding for things the SMTP interface
//! cannot express — configuration sets, per-message tags, richer send errors.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::OpenCompanyError;
use crate::server::ops::imap::ImapCredentials;
use crate::server::ops::smtp::{SmtpCredentials, SmtpSecurity};

/// Which transport a set of [`MailCredentials`] belongs to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailProvider {
    /// Any SMTP submission server. Also covers AWS SES, Mailgun, SendGrid, and
    /// Postmark via their SMTP endpoints.
    #[default]
    Smtp,
}

impl std::fmt::Display for MailProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MailProvider::Smtp => f.write_str("smtp"),
        }
    }
}

impl std::str::FromStr for MailProvider {
    type Err = OpenCompanyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "smtp" => Ok(MailProvider::Smtp),
            other => Err(OpenCompanyError::Config(format!(
                "unknown mail provider {other:?}; supported: smtp"
            ))),
        }
    }
}

/// Credentials for one mail provider — **secret**.
///
/// Tagged by `provider` on the wire so a stored blob is self-describing.
/// `Debug` is written by hand: a derived one would print the password into
/// whatever logged it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum MailCredentials {
    /// An SMTP submission server.
    Smtp(SmtpCredentials),
}

impl MailCredentials {
    /// Which transport these credentials need.
    pub fn provider(&self) -> MailProvider {
        match self {
            MailCredentials::Smtp(_) => MailProvider::Smtp,
        }
    }

    /// The envelope address mail sent with these credentials comes from.
    pub fn from_email(&self) -> &str {
        match self {
            MailCredentials::Smtp(c) => &c.from_email,
        }
    }

    /// The display name on the `From` header, if one is configured.
    pub fn from_name(&self) -> &str {
        match self {
            MailCredentials::Smtp(c) => &c.from_name,
        }
    }
}

impl std::fmt::Debug for MailCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the password. Mirrors AppConfig's hand-written Debug.
        f.debug_struct("MailCredentials")
            .field("provider", &self.provider())
            .field("from_email", &self.from_email())
            .finish_non_exhaustive()
    }
}

/// One outbound message handed to a [`MailSender`].
#[derive(Clone, Debug)]
pub struct OutboundEmail {
    /// Recipient address.
    pub to: String,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
}

/// The outbound-send seam.
///
/// Implementations are per-provider and select on the [`MailCredentials`]
/// variant. Mockable so every calling route is exercised offline; real
/// transports are feature-gated so the default build links no network crates.
#[async_trait]
pub trait MailSender: Send + Sync {
    /// Sends `email` using `creds`. An error means the message was not accepted.
    ///
    /// A sender handed credentials for a provider it does not implement must
    /// return [`OpenCompanyError::Config`] rather than panic — the binary may
    /// simply have been built without that provider's feature.
    async fn send(
        &self,
        creds: &MailCredentials,
        email: &OutboundEmail,
    ) -> Result<(), OpenCompanyError>;
}

/// One inbound message produced by a [`MailReceiver`]. Plain-text body (v1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundEmail {
    pub from_name: String,
    pub from_email: String,
    pub subject: String,
    pub body: String,
}

/// The inbound-fetch seam. Implementations fetch *new* (unseen) messages and
/// mark them seen, so a subsequent call returns only newer mail. Mockable so the
/// poller is exercised offline; the real transport is feature-gated.
#[async_trait]
pub trait MailReceiver: Send + Sync {
    async fn fetch_new(
        &self,
        creds: &ImapCredentials,
    ) -> Result<Vec<InboundEmail>, OpenCompanyError>;
}

/// Offline mock: returns queued batches, one per `fetch_new` call, and counts calls.
pub struct RecordingMailReceiver {
    batches: Mutex<std::collections::VecDeque<Vec<InboundEmail>>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl RecordingMailReceiver {
    pub fn new() -> Self {
        Self {
            batches: Mutex::new(std::collections::VecDeque::new()),
            calls: Default::default(),
        }
    }
    /// Queue a batch to be returned by the next `fetch_new`.
    pub fn push_batch(&self, batch: Vec<InboundEmail>) {
        self.batches.lock().expect("poisoned").push_back(batch);
    }
    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for RecordingMailReceiver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MailReceiver for RecordingMailReceiver {
    async fn fetch_new(
        &self,
        _creds: &ImapCredentials,
    ) -> Result<Vec<InboundEmail>, OpenCompanyError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self
            .batches
            .lock()
            .expect("poisoned")
            .pop_front()
            .unwrap_or_default())
    }
}

/// Host-level outbound mail configuration.
///
/// Resolved once, at boot, from `OPENCOMPANY_MAIL_*`. This is the platform's
/// own mail identity — the one login links are sent from. A company's own
/// outbound credentials live in its `SecretStore` instead, so a tenant is never
/// handed the platform's mail credential.
#[derive(Clone, Debug)]
pub struct MailConfig {
    /// The credentials to send platform mail with.
    pub credentials: MailCredentials,
}

impl MailConfig {
    /// Resolves host-level mail configuration from the environment.
    ///
    /// Returns `Ok(None)` when no provider is configured — mail is optional and
    /// its absence degrades a surface rather than failing a boot. But a
    /// *partial* configuration is an error, not a silent `None`: a deployment
    /// that set some of `OPENCOMPANY_MAIL_*` meant to have working mail, and
    /// discovering the typo when the first login link silently fails to arrive
    /// is worse than refusing here.
    pub fn from_env() -> Result<Option<Self>, OpenCompanyError> {
        let var = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());

        let provider: MailProvider = match var("OPENCOMPANY_MAIL_PROVIDER") {
            Some(raw) => raw.parse()?,
            // No provider named, but other mail vars present: default to smtp
            // rather than making PROVIDER=smtp boilerplate for the common case.
            None if var("OPENCOMPANY_MAIL_HOST").is_some() => MailProvider::Smtp,
            None => return Ok(None),
        };

        match provider {
            MailProvider::Smtp => {
                let missing = |key: &str| {
                    OpenCompanyError::Config(format!(
                        "{key} is required for OPENCOMPANY_MAIL_PROVIDER=smtp"
                    ))
                };
                let host =
                    var("OPENCOMPANY_MAIL_HOST").ok_or_else(|| missing("OPENCOMPANY_MAIL_HOST"))?;
                let from_email = var("OPENCOMPANY_MAIL_FROM_EMAIL")
                    .ok_or_else(|| missing("OPENCOMPANY_MAIL_FROM_EMAIL"))?;
                let port = match var("OPENCOMPANY_MAIL_PORT") {
                    Some(raw) => raw.parse::<u16>().map_err(|_| {
                        OpenCompanyError::Config(format!(
                            "OPENCOMPANY_MAIL_PORT must be a port number, got {raw:?}"
                        ))
                    })?,
                    None => 587,
                };
                let security = match var("OPENCOMPANY_MAIL_SECURITY") {
                    Some(raw) => match raw.trim().to_lowercase().as_str() {
                        "none" => SmtpSecurity::None,
                        "starttls" => SmtpSecurity::Starttls,
                        "ssl" | "tls" | "smtps" => SmtpSecurity::Ssl,
                        other => {
                            return Err(OpenCompanyError::Config(format!(
                                "unknown OPENCOMPANY_MAIL_SECURITY {other:?}; \
                                 supported: none, starttls, ssl"
                            )));
                        }
                    },
                    None => SmtpSecurity::default(),
                };
                Ok(Some(Self {
                    credentials: MailCredentials::Smtp(SmtpCredentials {
                        host,
                        port,
                        security,
                        username: var("OPENCOMPANY_MAIL_USERNAME").unwrap_or_default(),
                        password: var("OPENCOMPANY_MAIL_PASSWORD").unwrap_or_default(),
                        from_name: var("OPENCOMPANY_MAIL_FROM_NAME").unwrap_or_default(),
                        from_email,
                    }),
                }))
            }
        }
    }
}

/// A managed tenant's OWN mailbox identity, injected by the manager as
/// `OPENCOMPANY_MAIL_*`. Distinct from the host-level `OPENCOMPANY_MAIL_HOST/...`
/// platform-mail read by `MailConfig` (login links). Seeds the company's SMTP
/// send credentials AND the IMAP poller config.
#[derive(Clone)]
pub struct TenantMailboxConfig {
    pub address: String,
    pub smtp: SmtpCredentials,
    pub imap: ImapCredentials,
}

impl std::fmt::Debug for TenantMailboxConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the SMTP/IMAP passwords: `SmtpCredentials` derives Debug and
        // would otherwise print its password field through this struct.
        f.debug_struct("TenantMailboxConfig")
            .field("address", &self.address)
            .field("smtp_host", &self.smtp.host)
            .field("imap_host", &self.imap.host)
            .finish_non_exhaustive()
    }
}

impl TenantMailboxConfig {
    /// `Ok(None)` when unconfigured (no `OPENCOMPANY_MAIL_ADDRESS`); a *partial*
    /// injection is a hard error.
    pub fn from_env() -> Result<Option<Self>, OpenCompanyError> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let Some(address) = var("OPENCOMPANY_MAIL_ADDRESS") else {
            return Ok(None);
        };
        let need = |k: &str| {
            var(k).ok_or_else(|| {
                OpenCompanyError::Config(format!(
                    "{k} is required when OPENCOMPANY_MAIL_ADDRESS is set"
                ))
            })
        };
        let port = |k: &str| -> Result<u16, OpenCompanyError> {
            need(k)?
                .parse::<u16>()
                .map_err(|_| OpenCompanyError::Config(format!("{k} must be a port number")))
        };
        let user = need("OPENCOMPANY_MAIL_USER")?;
        let password = need("OPENCOMPANY_MAIL_PASSWORD")?;
        let smtp = SmtpCredentials {
            host: need("OPENCOMPANY_MAIL_SMTP_HOST")?,
            port: port("OPENCOMPANY_MAIL_SMTP_PORT")?,
            security: SmtpSecurity::default(),
            username: user.clone(),
            password: password.clone(),
            from_name: String::new(),
            from_email: address.clone(),
        };
        let imap = ImapCredentials {
            host: need("OPENCOMPANY_MAIL_IMAP_HOST")?,
            port: port("OPENCOMPANY_MAIL_IMAP_PORT")?,
            username: user,
            password,
        };
        Ok(Some(Self {
            address,
            smtp,
            imap,
        }))
    }
}

/// An offline mock sender that records every send and never fails. Used by
/// tests and any offline deployment.
#[derive(Clone, Default)]
pub struct RecordingMailSender {
    sent: Arc<Mutex<Vec<(String, OutboundEmail)>>>,
}

impl RecordingMailSender {
    /// Creates an empty recording sender.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every `(from_email, email)` sent so far.
    pub fn sent(&self) -> Vec<(String, OutboundEmail)> {
        self.sent.lock().expect("mail sender poisoned").clone()
    }
}

#[async_trait]
impl MailSender for RecordingMailSender {
    async fn send(
        &self,
        creds: &MailCredentials,
        email: &OutboundEmail,
    ) -> Result<(), OpenCompanyError> {
        self.sent
            .lock()
            .expect("mail sender poisoned")
            .push((creds.from_email().to_string(), email.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Serializes tests that read/write process env (env-reading tests must
    /// run single-threaded: `cargo test mailer:: -- --test-threads=1`).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn smtp_creds() -> SmtpCredentials {
        SmtpCredentials {
            host: "smtp.example.com".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "user".into(),
            password: "hunter2".into(),
            from_name: "Acme".into(),
            from_email: "hi@acme.test".into(),
        }
    }

    #[test]
    fn credentials_are_tagged_by_provider_on_the_wire() {
        let creds = MailCredentials::Smtp(smtp_creds());
        let json = serde_json::to_value(&creds).unwrap();
        // The tag is what lets a stored blob name its own transport.
        assert_eq!(json["provider"], "smtp");
        assert_eq!(json["host"], "smtp.example.com");
        let back: MailCredentials = serde_json::from_value(json).unwrap();
        assert_eq!(back.provider(), MailProvider::Smtp);
        assert_eq!(back.from_email(), "hi@acme.test");
    }

    #[test]
    fn debug_never_prints_the_password() {
        let creds = MailCredentials::Smtp(smtp_creds());
        let rendered = format!("{creds:?}");
        assert!(
            !rendered.contains("hunter2"),
            "the password leaked into Debug: {rendered}"
        );
        // Still identifies itself usefully. `MailProvider`'s derived Debug is
        // the variant name; the lowercase spelling is the serde/Display form.
        assert!(rendered.contains("Smtp"), "unhelpful Debug: {rendered}");
        assert!(rendered.contains("hi@acme.test"));
    }

    #[test]
    fn smtp_credentials_debug_still_leaks_so_never_derive_it_upward() {
        // A guard-rail, not an endorsement: SmtpCredentials derives Debug and
        // prints its password. That is tolerable only because the type is
        // confined to the SecretStore. If this ever starts passing, the derive
        // was fixed and MailCredentials' hand-written Debug could be relaxed —
        // until then, nothing may put SmtpCredentials in a Debug-printed struct.
        let rendered = format!("{:?}", smtp_creds());
        assert!(
            rendered.contains("hunter2"),
            "SmtpCredentials::Debug no longer leaks; revisit this guard"
        );
    }

    #[test]
    fn provider_parses_and_rejects_unknown() {
        assert_eq!("smtp".parse::<MailProvider>().unwrap(), MailProvider::Smtp);
        assert_eq!(
            "  SMTP ".parse::<MailProvider>().unwrap(),
            MailProvider::Smtp
        );
        let err = "ses".parse::<MailProvider>().unwrap_err();
        assert_eq!(err.code(), "config_error");
        // The message should name what IS supported, not just what isn't.
        assert!(format!("{err}").contains("smtp"));
    }

    #[tokio::test]
    async fn recording_sender_captures_the_envelope_sender() {
        let sender = RecordingMailSender::new();
        let creds = MailCredentials::Smtp(smtp_creds());
        sender
            .send(
                &creds,
                &OutboundEmail {
                    to: "ada@example.com".into(),
                    subject: "hi".into(),
                    body: "body".into(),
                },
            )
            .await
            .unwrap();
        let sent = sender.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "hi@acme.test");
        assert_eq!(sent[0].1.to, "ada@example.com");
    }

    #[tokio::test]
    async fn recording_receiver_returns_queued_batches_in_order() {
        let creds = ImapCredentials {
            host: "h".into(),
            port: 993,
            username: "u".into(),
            password: "p".into(),
        };
        let rx = RecordingMailReceiver::new();
        rx.push_batch(vec![InboundEmail {
            from_name: "A".into(),
            from_email: "a@x".into(),
            subject: "s".into(),
            body: "b".into(),
        }]);
        assert_eq!(rx.fetch_new(&creds).await.unwrap().len(), 1);
        assert_eq!(rx.fetch_new(&creds).await.unwrap().len(), 0); // drained
        assert_eq!(rx.calls(), 2);
    }

    #[test]
    fn tenant_mailbox_config_parses_injected_env() {
        // Serialize env access; set the 7 injected vars.
        let _g = ENV_LOCK.lock().unwrap();
        for (k, v) in [
            ("OPENCOMPANY_MAIL_ADDRESS", "acme@opencompany.work"),
            ("OPENCOMPANY_MAIL_SMTP_HOST", "mail.opencompany.work"),
            ("OPENCOMPANY_MAIL_SMTP_PORT", "465"),
            ("OPENCOMPANY_MAIL_IMAP_HOST", "mail.opencompany.work"),
            ("OPENCOMPANY_MAIL_IMAP_PORT", "993"),
            ("OPENCOMPANY_MAIL_USER", "acme@opencompany.work"),
            ("OPENCOMPANY_MAIL_PASSWORD", "secret"),
        ] {
            unsafe { std::env::set_var(k, v) };
        }

        let cfg = TenantMailboxConfig::from_env()
            .unwrap()
            .expect("configured");
        assert_eq!(cfg.address, "acme@opencompany.work");
        assert_eq!(cfg.imap.host, "mail.opencompany.work");
        assert_eq!(cfg.imap.port, 993);
        assert_eq!(cfg.smtp.from_email, "acme@opencompany.work");

        for k in [
            "OPENCOMPANY_MAIL_ADDRESS",
            "OPENCOMPANY_MAIL_SMTP_HOST",
            "OPENCOMPANY_MAIL_SMTP_PORT",
            "OPENCOMPANY_MAIL_IMAP_HOST",
            "OPENCOMPANY_MAIL_IMAP_PORT",
            "OPENCOMPANY_MAIL_USER",
            "OPENCOMPANY_MAIL_PASSWORD",
        ] {
            unsafe { std::env::remove_var(k) };
        }
    }

    #[test]
    fn tenant_mailbox_config_absent_is_none() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var("OPENCOMPANY_MAIL_ADDRESS") };
        assert!(TenantMailboxConfig::from_env().unwrap().is_none());
    }

    #[test]
    fn tenant_mailbox_config_partial_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("OPENCOMPANY_MAIL_ADDRESS", "acme@opencompany.work") };
        unsafe { std::env::remove_var("OPENCOMPANY_MAIL_PASSWORD") };
        assert!(TenantMailboxConfig::from_env().is_err());
        unsafe { std::env::remove_var("OPENCOMPANY_MAIL_ADDRESS") };
    }
}
