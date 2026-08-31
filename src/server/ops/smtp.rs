//! A company's own SMTP credentials + test send, and the `lettre` transport.
//!
//! This is the SMTP-specific half of outbound mail; the provider-agnostic
//! adapter it plugs into lives in [`mailer`](super::mailer).
//!
//! `PUT …/smtp` stores credentials in [`SecretStore`](crate::ports::SecretStore)
//! and returns a non-secret [`SmtpStatus`] — the password never appears in any
//! response. They are stored under two keys, not one: the configuration under
//! [`SMTP_KEY`](super::SMTP_KEY) and the password under
//! [`SMTP_PASSWORD_KEY`](super::SMTP_PASSWORD_KEY). That split is what lets a
//! save that omits the password keep the stored one without reading it first —
//! see [`put_smtp`]. `POST …/smtp/test`
//! sends a test email through the mockable
//! [`MailSender`](super::mailer::MailSender) seam, pulling the stored
//! credentials per send, and records the sent mail in the company's
//! [`InboxStore`](crate::ports::InboxStore) so the console shows it. The real
//! `lettre` transport is gated behind the `smtp` feature; without an injected
//! sender the test route is "not wired yet" (404).
//!
//! These are the *company's* credentials, distinct from the host-level ones in
//! [`MailConfig`](super::mailer::MailConfig) that platform mail uses.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::inbox::EmailRecord;
use crate::ports::types::{CompanyId, SecretValue};
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::ops::mailer::{MailCredentials, OutboundEmail};
use crate::server::ops::{AdminScopedCompany, SMTP_KEY, SMTP_PASSWORD_KEY, ScopedCompany, scoped};

/// The SMTP security mode. Mirrors the console's `SmtpSecurity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    /// No transport security.
    None,
    /// Opportunistic STARTTLS on the submission port.
    #[default]
    Starttls,
    /// Implicit TLS (SMTPS).
    Ssl,
}

/// The full SMTP credentials — **secret**. Persisted only to
/// [`SecretStore`](crate::ports::SecretStore); never serialized into a route
/// response.
///
/// The password is a [`SecretValue`], so the derived `Debug` and `Serialize`
/// are both redacted by the field's type (issue #1770). Until then this struct
/// derived both over a plain `String` and leaked through either — a leak
/// `mailer::test::smtp_credentials_debug_still_leaks_so_never_derive_it_upward`
/// existed only to document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SmtpCredentials {
    /// SMTP server host.
    pub host: String,
    /// SMTP server port.
    pub port: u16,
    /// Transport security mode.
    #[serde(default)]
    pub security: SmtpSecurity,
    /// Login username.
    pub username: String,
    /// Login password (secret).
    pub password: SecretValue,
    /// Display name on the `From` header.
    #[serde(default)]
    pub from_name: String,
    /// Envelope/from address.
    pub from_email: String,
}

/// The non-secret status of a company's SMTP configuration. The password is
/// intentionally absent — a response never carries credential material.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SmtpStatus {
    /// Whether SMTP credentials are stored.
    pub configured: bool,
    /// SMTP host, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// SMTP port, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Security mode, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<SmtpSecurity>,
    /// Login username, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// From display name, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
    /// From address, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_email: Option<String>,
}

impl SmtpStatus {
    /// Projects credentials to their non-secret status. Drops the password.
    pub fn from_credentials(creds: &SmtpCredentials) -> Self {
        Self {
            configured: true,
            host: Some(creds.host.clone()),
            port: Some(creds.port),
            security: Some(creds.security),
            username: Some(creds.username.clone()),
            from_name: Some(creds.from_name.clone()),
            from_email: Some(creds.from_email.clone()),
        }
    }

    /// The "nothing stored" status.
    pub fn unconfigured() -> Self {
        Self {
            configured: false,
            host: None,
            port: None,
            security: None,
            username: None,
            from_name: None,
            from_email: None,
        }
    }
}

/// Builds the SMTP route fragment.
pub fn router() -> Router<AppState> {
    scoped("/smtp", get(get_smtp).put(put_smtp)).merge(scoped("/smtp/test", post(test_smtp)))
}

// -- GET smtp ---------------------------------------------------------------

/// `GET …/smtp` (both scope forms) — the non-secret status of what is stored.
///
/// `ScopedCompany`, and the asymmetry with its neighbours is deliberate. The
/// admin line on this plane guards the company's outward identity: `PUT …/smtp`
/// sets the credentials its mail goes out under, and `POST …/smtp/test` sends
/// real mail to a recipient the caller names in the body. This read does
/// neither — [`SmtpStatus`] is host, port, username and from-address, with the
/// password absent by construction — so it stays open to any member, the same
/// rule `docs/modules/server/authority.md` already states for reads on these
/// surfaces. Admin-only, it would `403` a member on the Settings screen while
/// the identical projection stayed readable to them over GraphQL as
/// `Company.smtp`.
///
/// [`SmtpStatus::unconfigured`] rather than `null` when nothing is stored: the
/// type already carries a `configured` flag to say so, and the GraphQL field is
/// the non-null `SmtpStatus!`.
async fn get_smtp(company: ScopedCompany) -> Result<Json<SmtpStatus>, ApiError> {
    let stored = load_config(&company.runtime).await?;
    Ok(Json(
        stored.map_or_else(SmtpStatus::unconfigured, |config| config.status()),
    ))
}

// -- PUT smtp ---------------------------------------------------------------

/// What lives under [`SMTP_KEY`]: the credentials minus the password, which has
/// its own key ([`SMTP_PASSWORD_KEY`]).
///
/// `password` is a **legacy read path only**. Blobs written before the split
/// embedded it here, so [`load_config`] still parses it and [`load_credentials`]
/// still falls back to it.
///
/// # Why it cannot be written back (issue #1770)
///
/// It used to be safe only because `skip_serializing_if = "Option::is_none"`
/// met a single construction site ([`put_smtp`]) that hardcoded `None`. That is
/// a construction-site invariant, not a guard: the day somebody adds a second
/// construction site, [`store_config`] writes a live credential into the
/// configuration blob and nothing fails. Two type-level guards replace it, and
/// neither depends on how the struct is built:
///
/// - `#[serde(skip_serializing)]` — serde has no code path that emits this
///   field, whatever it holds. The key is absent from the written blob rather
///   than absent-when-`None`.
/// - [`SecretValue`] — even with that attribute removed, the value that
///   reached the blob would be `"[redacted]"` rather than the password.
///
/// The second matters on its own terms: writing `"[redacted]"` back into the
/// blob would also poison [`load_credentials`]'s legacy fallback, which reads
/// this field when [`SMTP_PASSWORD_KEY`] is empty.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct StoredConfig {
    /// SMTP server host.
    host: String,
    /// SMTP server port.
    port: u16,
    /// Transport security mode.
    #[serde(default)]
    security: SmtpSecurity,
    /// Login username.
    username: String,
    /// The pre-split password location. Read, **never** written — see the type
    /// docs. `Debug` is safe by the field's type, so this struct no longer
    /// hand-writes one.
    #[serde(default, skip_serializing)]
    password: Option<SecretValue>,
    /// Display name on the `From` header.
    #[serde(default)]
    from_name: String,
    /// Envelope/from address.
    from_email: String,
}

impl StoredConfig {
    /// Projects the stored configuration to its non-secret status.
    fn status(&self) -> SmtpStatus {
        SmtpStatus {
            configured: true,
            host: Some(self.host.clone()),
            port: Some(self.port),
            security: Some(self.security),
            username: Some(self.username.clone()),
            from_name: Some(self.from_name.clone()),
            from_email: Some(self.from_email.clone()),
        }
    }
}

/// Reads the stored configuration blob, if any.
async fn load_config(runtime: &CompanyRuntime) -> Result<Option<StoredConfig>, OpenCompanyError> {
    let Some(value) = runtime.secrets().get(runtime.id(), SMTP_KEY).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(value.expose())?))
}

/// Writes the password to its own key.
async fn store_password(runtime: &CompanyRuntime, password: &str) -> Result<(), OpenCompanyError> {
    runtime
        .secrets()
        .set(
            runtime.id(),
            SMTP_PASSWORD_KEY,
            SecretValue(password.to_string()),
        )
        .await
}

/// Persists the configuration blob and returns the non-secret status.
async fn store_config(
    runtime: Arc<CompanyRuntime>,
    config: StoredConfig,
) -> Result<Json<SmtpStatus>, ApiError> {
    let json = serde_json::to_string(&config)?;
    runtime
        .secrets()
        .set(runtime.id(), SMTP_KEY, SecretValue(json))
        .await?;
    Ok(Json(config.status()))
}

/// The save-credentials body: [`SmtpCredentials`], except that the password is
/// a **patch**.
///
/// Same shape and same field names on the wire — a body carrying a password
/// behaves exactly as it always did. What is new is that a body *without* one
/// keeps the stored password, mirroring the patch semantics
/// [`hosting`](super::hosting) already uses for its API key. The reason is the
/// same: the password is write-only, so a form can never render it back, and
/// without this every correction to a from-name would cost the operator a
/// credential they would have to go and look up again.
///
/// Field names stay `snake_case` (`from_name`, `from_email`) because
/// [`SmtpCredentials`] has no `rename_all` and the console mirrors it as-is.
#[derive(Debug, Deserialize)]
struct SmtpConfigBody {
    /// SMTP server host.
    host: String,
    /// SMTP server port.
    port: u16,
    /// Transport security mode.
    #[serde(default)]
    security: SmtpSecurity,
    /// Login username.
    username: String,
    /// Login password. Omit (or send empty) to keep the stored one.
    #[serde(default)]
    password: Option<String>,
    /// Display name on the `From` header.
    #[serde(default)]
    from_name: String,
    /// Envelope/from address.
    from_email: String,
}

/// `PUT …/smtp` (both scope forms).
///
/// Requires authority over the company (issue #403): these credentials are the
/// address the company's mail goes out as.
///
/// Every field but the password replaces what is stored. The password is kept
/// when the body omits it, so "stored — leave blank to keep" is a save the
/// console can actually offer; with nothing supplied and nothing stored, the
/// request is refused rather than persisting credentials that could never
/// authenticate.
///
/// The password is taken **byte for byte** as supplied. Trimming is only ever
/// used to decide whether the caller supplied one at all: an SMTP password may
/// legitimately open or close with a space, and silently storing `" hunter2 "`
/// as `"hunter2"` would fail authentication with nothing in any response or log
/// to explain why.
///
/// Keeping the stored password is the *absence* of a write, not a
/// read-modify-write: the two live under separate keys ([`SMTP_KEY`] and
/// [`SMTP_PASSWORD_KEY`]), so a passwordless save rewrites the configuration
/// blob and leaves the secret untouched. A rotation landing concurrently is
/// therefore preserved rather than reverted. The one exception is credentials
/// written before the split, whose password still sits inside the blob: the
/// first passwordless save after the split has to read it and write it to its
/// own key, which is a genuine read-modify-write. That one path is serialized
/// by [`write_lock`] so a rotation cannot interleave with it.
async fn put_smtp(
    company: AdminScopedCompany,
    Json(body): Json<SmtpConfigBody>,
) -> Result<Json<SmtpStatus>, ApiError> {
    // Held across the whole handler, so the legacy migration below and any
    // rotation racing it cannot interleave. The steady-state path needs no lock
    // — it is raceless by construction — but taking it unconditionally keeps
    // the ordering rule in one place rather than in each branch.
    let lock = write_lock(company.runtime.id());
    let _guard = lock.lock().await;
    match body.password.filter(|password| !password.trim().is_empty()) {
        // A rotation. Write the secret before the configuration blob: the blob
        // is what makes the company read as `configured`, so this order can
        // never leave a configured company pointing at no password.
        Some(password) => store_password(&company.runtime, &password).await?,
        // A passwordless save. Confirm a password exists — migrating a
        // pre-split one to its own key — and otherwise leave it alone.
        None => ensure_password_stored(&company.runtime).await?,
    }
    let config = StoredConfig {
        host: body.host,
        port: body.port,
        security: body.security,
        username: body.username,
        password: None,
        from_name: body.from_name,
        from_email: body.from_email,
    };
    store_config(company.runtime, config).await
}

/// Per-company serialization for `PUT …/smtp`.
///
/// The split-key layout already makes the steady-state passwordless save
/// raceless without any lock, because it writes no password at all. This exists
/// for the one path that must still read and then write — migrating a pre-split
/// password out of the configuration blob — where a rotation landing in between
/// would otherwise be overwritten by the migration.
///
/// **In-process only, and deliberately so.** A tenant runs as a single
/// container ([`docs/spec/runtime/storage.md`]), so one process is the whole
/// population of concurrent writers in the deployed topology. Two replicas of
/// one company would reopen the window on the legacy path alone; closing it
/// there would need a conditional write in
/// [`SecretStore`](crate::ports::SecretStore), which the port cannot express
/// today. The steady-state path stays correct under any number of replicas.
fn write_lock(company: &CompanyId) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<CompanyId, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let locks = LOCKS.get_or_init(Default::default);
    let mut locks = locks.lock().expect("smtp write locks poisoned");
    Arc::clone(
        locks
            .entry(company.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

/// Confirms a password is stored ahead of a passwordless save, and refuses the
/// save when none is.
///
/// In the steady state this reads one key and writes nothing, which is what
/// keeps the passwordless save free of a read-modify-write. It writes only to
/// migrate a pre-split password out of the configuration blob, because the
/// blob is about to be rewritten without it.
async fn ensure_password_stored(runtime: &CompanyRuntime) -> Result<(), ApiError> {
    if runtime
        .secrets()
        .get(runtime.id(), SMTP_PASSWORD_KEY)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let legacy = load_config(runtime)
        .await?
        .and_then(|config| config.password);
    let legacy = legacy.ok_or_else(|| {
        ApiError(OpenCompanyError::InvalidRequest(
            "an SMTP password is required".to_string(),
        ))
    })?;
    store_password(runtime, legacy.expose()).await?;
    Ok(())
}

// -- POST smtp/test ---------------------------------------------------------

/// The optional test-send override.
#[derive(Debug, Default, Deserialize)]
struct TestSend {
    /// Recipient; defaults to the configured `from_email` (loopback test).
    #[serde(default)]
    to: Option<String>,
}

/// The test-send result.
#[derive(Debug, Serialize)]
struct TestResult {
    /// Whether the send was accepted.
    ok: bool,
    /// A prosumer-friendly description of the outcome.
    message: String,
}

/// Sends a test email through the injected sender and records it as outbound.
async fn run_test(
    state: &AppState,
    runtime: Arc<CompanyRuntime>,
    body: TestSend,
) -> Result<Json<TestResult>, crate::server::Rejection> {
    use axum::response::IntoResponse;
    // Not wired without a sender (default build / no `smtp` feature).
    let Some(sender) = state.connections().mail.clone() else {
        return Err(super::not_wired("smtp test send").into());
    };
    let creds = load_credentials(&runtime).await?;
    let Some(creds) = creds else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "no SMTP credentials configured".to_string(),
        ))
        .into_response()
        .into());
    };
    let to = body.to.unwrap_or_else(|| creds.from_email.clone());
    let email = OutboundEmail {
        to: to.clone(),
        subject: "OpenCompany SMTP test".to_string(),
        body: "This is a test message confirming your outbound email is wired up.".to_string(),
    };
    // The company's stored credentials are SMTP by construction (the route that
    // writes them is `PUT …/smtp`), so tag them for the provider-agnostic seam.
    let tagged = MailCredentials::Smtp(creds.clone());
    match sender.send(&tagged, &email).await {
        Ok(()) => {
            record_outbound(&runtime, &creds, &email).await;
            Ok(Json(TestResult {
                ok: true,
                message: format!("Test email sent to {to}."),
            }))
        }
        Err(err) => Ok(Json(TestResult {
            ok: false,
            message: format!("Send failed: {err}"),
        })),
    }
}

/// Loads stored SMTP credentials, if any, recombining the configuration blob
/// with the separately stored password.
///
/// The password comes from [`SMTP_PASSWORD_KEY`], falling back to the copy
/// inside the blob for credentials written before the two were split. Only the
/// send path needs this; [`get_smtp`] reads the configuration alone so that
/// rendering the settings page never touches the secret.
pub(crate) async fn load_credentials(
    runtime: &CompanyRuntime,
) -> Result<Option<SmtpCredentials>, OpenCompanyError> {
    let Some(config) = load_config(runtime).await? else {
        return Ok(None);
    };
    let password = match runtime
        .secrets()
        .get(runtime.id(), SMTP_PASSWORD_KEY)
        .await?
    {
        Some(value) => value,
        None => config
            .password
            .clone()
            .unwrap_or_else(|| SecretValue(String::new())),
    };
    Ok(Some(SmtpCredentials {
        host: config.host,
        port: config.port,
        security: config.security,
        username: config.username,
        password,
        from_name: config.from_name,
        from_email: config.from_email,
    }))
}

/// Appends a sent email to the sender's inbox so the console shows outbound mail.
pub(crate) async fn record_outbound(
    runtime: &CompanyRuntime,
    creds: &SmtpCredentials,
    email: &OutboundEmail,
) {
    let record = EmailRecord {
        id: generate_id(),
        inbox: local_part(&creds.from_email),
        from_name: creds.from_name.clone(),
        from_email: creds.from_email.clone(),
        subject: email.subject.clone(),
        body: email.body.clone(),
        at_millis: now_millis(),
        read: true,
        outbound: true,
    };
    if let Err(err) = runtime.inbox().append(runtime.id(), &record).await {
        tracing::warn!(company = %runtime.id(), "failed to record outbound email: {err}");
    }
}

/// The local part of an address (`ceo@acme.test` → `ceo`), or the whole string
/// when it carries no `@`.
///
/// `pub` (not `pub(crate)`) so the `opencompany` binary target — a separate
/// crate from this library — can reuse it to scope an injected mailbox to its
/// owning company (see `spawn_mailbox_poller`/`register_company` in
/// `src/bin/opencompany.rs`).
pub fn local_part(address: &str) -> String {
    address
        .split_once('@')
        .map(|(local, _)| local.to_string())
        .unwrap_or_else(|| address.to_string())
}

/// `POST …/smtp/test` (both scope forms).
///
/// Requires authority over the company (issue #403). Grouped with the write
/// rather than with the read-only probes because the caller chooses the
/// recipient: it sends real mail from the company's address to an address
/// supplied in the request body.
async fn test_smtp(
    company: AdminScopedCompany,
    State(state): State<AppState>,
    body: Option<Json<TestSend>>,
) -> Result<Json<TestResult>, crate::server::Rejection> {
    run_test(
        &state,
        company.runtime,
        body.map(|b| b.0).unwrap_or_default(),
    )
    .await
}

/// The real `lettre` SMTP transport. Gated behind the `smtp` feature so the
/// default build links no SMTP crate.
#[cfg(feature = "smtp")]
pub struct LettreMailSender;

/// Upper bound on one delivery (connect through the final response).
///
/// `lettre`'s own timeout bounds individual phases and not reliably all of
/// them, so a relay that accepts a connection and then stalls — mid-TLS, mid
/// -write, or before its reply — can hold a caller far longer than the caller
/// budgeted for. Every send here happens inside an HTTP request an operator is
/// waiting on, so the bound lives next to the socket rather than at each call
/// site: one place, and no call site can forget it. Same placement, and same
/// reason, as `IMAP_TIMEOUT` in `imap.rs`.
#[cfg(feature = "smtp")]
const SMTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(feature = "smtp")]
#[async_trait::async_trait]
impl crate::server::ops::mailer::MailSender for LettreMailSender {
    async fn send(
        &self,
        creds: &MailCredentials,
        email: &OutboundEmail,
    ) -> Result<(), OpenCompanyError> {
        match tokio::time::timeout(SMTP_TIMEOUT, Self::send_inner(creds, email)).await {
            Ok(result) => result,
            // The same variant a refused send reports, because it means the
            // same thing to every caller: the message was not accepted, and
            // nothing may be recorded as delivered.
            Err(_) => Err(OpenCompanyError::Store("smtp send: timed out".into())),
        }
    }
}

#[cfg(feature = "smtp")]
impl LettreMailSender {
    async fn send_inner(
        creds: &MailCredentials,
        email: &OutboundEmail,
    ) -> Result<(), OpenCompanyError> {
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

        // Selecting the transport by variant is what makes this an adapter: a
        // future provider adds a variant, and this match stops compiling until
        // someone decides what this sender does with it.
        let MailCredentials::Smtp(creds) = creds;

        let from = if creds.from_name.is_empty() {
            creds.from_email.clone()
        } else {
            format!("{} <{}>", creds.from_name, creds.from_email)
        };
        let message = Message::builder()
            .from(from.parse().map_err(|e| {
                OpenCompanyError::InvalidRequest(format!("invalid from address: {e}"))
            })?)
            .to(email.to.parse().map_err(|e| {
                OpenCompanyError::InvalidRequest(format!("invalid to address: {e}"))
            })?)
            .subject(&email.subject)
            .body(email.body.clone())
            .map_err(|e| OpenCompanyError::Store(format!("build message: {e}")))?;

        let mut builder = match creds.security {
            SmtpSecurity::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&creds.host)
                    .port(creds.port)
            }
            SmtpSecurity::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&creds.host)
                    .map_err(|e| OpenCompanyError::Store(format!("smtp starttls: {e}")))?
                    .port(creds.port)
            }
            SmtpSecurity::Ssl => AsyncSmtpTransport::<Tokio1Executor>::relay(&creds.host)
                .map_err(|e| OpenCompanyError::Store(format!("smtp relay: {e}")))?
                .port(creds.port),
        };
        // An empty username means the relay takes unauthenticated mail, so do
        // not configure credentials — lettre would otherwise attempt AUTH with
        // an empty secret and fail on a listener that advertises no mechanism.
        // The CI Stalwart fixture's plaintext port 25 is exactly that listener.
        if !creds.username.is_empty() {
            builder = builder.credentials(Credentials::new(
                creds.username.clone(),
                creds.password.expose().to_string(),
            ));
        }
        let transport = builder.build();
        transport
            .send(message)
            .await
            .map_err(|e| OpenCompanyError::Store(format!("smtp send: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn status_drops_password() {
        let creds = SmtpCredentials {
            host: "smtp.example.com".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "user".into(),
            password: SecretValue("s3cret-pw".into()),
            from_name: "Acme".into(),
            from_email: "ceo@acme.test".into(),
        };
        let status = SmtpStatus::from_credentials(&creds);
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("s3cret-pw"), "password leaked into status");
        assert!(json.contains("smtp.example.com"));
        assert!(status.configured);
    }

    #[test]
    fn local_part_splits_address() {
        assert_eq!(local_part("ceo@acme.test"), "ceo");
        assert_eq!(local_part("bare"), "bare");
    }

    #[tokio::test]
    async fn recording_sender_captures_send() {
        use crate::server::ops::mailer::{MailSender, RecordingMailSender};

        let sender = RecordingMailSender::new();
        let creds = MailCredentials::Smtp(SmtpCredentials {
            host: "h".into(),
            port: 25,
            security: SmtpSecurity::None,
            username: "u".into(),
            password: SecretValue("p".into()),
            from_name: String::new(),
            from_email: "from@x.test".into(),
        });
        let email = OutboundEmail {
            to: "to@x.test".into(),
            subject: "s".into(),
            body: "b".into(),
        };
        sender.send(&creds, &email).await.unwrap();
        assert_eq!(sender.sent().len(), 1);
        assert_eq!(sender.sent()[0].0, "from@x.test");
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    /// Obviously fake, and distinctive enough that a substring hit is a real
    /// hit. Same sentinel as the other planted-secret tests in this crate.
    const FAKE_SECRET: &str = "NOT-A-REAL-KEY-planted-for-tests";

    /// Case-**insensitive**: a leak that arrives lowercased, uppercased or
    /// otherwise case-mangled is still a leak, and an exact-case search reads
    /// it as clean.
    fn leaks(rendering: &str) -> bool {
        rendering
            .to_ascii_lowercase()
            .contains(&FAKE_SECRET.to_ascii_lowercase())
    }

    /// Sanity: the detector detects. Without this, every assertion built on
    /// [`leaks`] could be vacuous and still read as green.
    #[test]
    fn the_leak_detector_can_see_a_lowercased_sentinel() {
        assert!(
            leaks(&format!("password={}", FAKE_SECRET.to_ascii_lowercase())),
            "the leak detector cannot see a lowercased sentinel; every \
             assertion in this module would be vacuous"
        );
        assert!(!leaks("password=nothing-planted-here"));
    }

    /// Issue #1770. `SmtpCredentials` derived both `Debug` and `Serialize`
    /// over a plain `String` password, so either surface emitted the
    /// plaintext — the `Debug` half was documented as a known leak rather than
    /// fixed, and the `Serialize` half was not considered at all.
    ///
    /// The assertions go through containers this struct knows nothing about,
    /// because the guard now lives on the field's *type*: [`MailCredentials`]
    /// (an externally tagged enum), a struct with a plain
    /// `#[derive(Serialize)]`, plus `Option`, `Vec`, a map value and
    /// `#[serde(flatten)]` — a genuinely different serde code path — across
    /// both `to_string` and `to_value`, and both `{:?}` and `{:#?}`.
    #[test]
    fn planted_password_never_reaches_debug_or_serialize() {
        use std::collections::BTreeMap;

        /// The next struct somebody writes: derives `Serialize` and `Debug`
        /// with no idea a credential is in there.
        #[derive(Debug, Serialize)]
        struct UnsuspectingConfig {
            label: String,
            primary: SmtpCredentials,
            optional: Option<SmtpCredentials>,
            many: Vec<SmtpCredentials>,
            by_name: BTreeMap<String, SmtpCredentials>,
            tagged: MailCredentials,
            #[serde(flatten)]
            nested: Nested,
        }

        /// Flattened, so serde uses `FlatMapSerializer` rather than the
        /// ordinary struct serializer.
        #[derive(Debug, Serialize)]
        struct Nested {
            inner: SmtpCredentials,
        }

        let creds = SmtpCredentials {
            host: "smtp.example.com".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: "mailer".into(),
            password: SecretValue(FAKE_SECRET.to_string()),
            from_name: "Acme".into(),
            from_email: "ceo@acme.test".into(),
        };
        let config = UnsuspectingConfig {
            label: "company mail".to_string(),
            primary: creds.clone(),
            optional: Some(creds.clone()),
            many: vec![creds.clone(), creds.clone()],
            by_name: BTreeMap::from([("acme".to_string(), creds.clone())]),
            tagged: MailCredentials::Smtp(creds.clone()),
            nested: Nested {
                inner: creds.clone(),
            },
        };

        for rendering in [
            serde_json::to_string(&creds).expect("serializes"),
            serde_json::to_value(&creds)
                .expect("serializes")
                .to_string(),
            serde_json::to_string(&config).expect("serializes"),
            serde_json::to_value(&config)
                .expect("serializes")
                .to_string(),
        ] {
            assert!(!leaks(&rendering), "plaintext reached serde: {rendering}");
        }
        for rendering in [
            format!("{creds:?}"),
            format!("{creds:#?}"),
            format!("{config:?}"),
            format!("{config:#?}"),
        ] {
            assert!(!leaks(&rendering), "plaintext reached Debug: {rendering}");
        }

        // Still diagnosable, and the credential still reachable by the one
        // named door so the transport can still authenticate.
        let rendered = format!("{creds:?}");
        assert!(rendered.contains("smtp.example.com"), "{rendered}");
        assert!(rendered.contains("ceo@acme.test"), "{rendered}");
        assert_eq!(creds.password.expose(), FAKE_SECRET);
    }

    /// Issue #1770, the half that is about *persistence* rather than logging.
    ///
    /// [`StoredConfig`] is written back to [`SMTP_KEY`] by [`store_config`] on
    /// every save. Its legacy `password` was safe only because
    /// `skip_serializing_if = "Option::is_none"` met one construction site that
    /// hardcoded `None` — an invariant a second construction site would break
    /// silently. Both guards that replaced it are asserted here against a
    /// `StoredConfig` that *is* carrying a legacy password, which is exactly
    /// the state [`load_config`] produces when it reads a pre-split blob.
    #[test]
    fn a_legacy_password_is_never_written_back_into_the_stored_blob() {
        let stored: StoredConfig = serde_json::from_value(serde_json::json!({
            "host": "smtp.acme.test",
            "port": 587,
            "security": "starttls",
            "username": "mailer",
            "password": FAKE_SECRET,
            "from_name": "Acme",
            "from_email": "ceo@acme.test",
        }))
        .expect("a pre-split blob still parses");

        // It really did load — otherwise the assertions below pass on a `None`
        // and prove nothing.
        assert_eq!(
            stored.password.as_ref().map(SecretValue::expose),
            Some(FAKE_SECRET),
            "the legacy read path stopped working, so this test is vacuous"
        );

        let as_string = serde_json::to_string(&stored).expect("serializes");
        let as_value = serde_json::to_value(&stored).expect("serializes");

        for rendering in [
            as_string.clone(),
            as_value.to_string(),
            format!("{stored:?}"),
            format!("{stored:#?}"),
        ] {
            assert!(
                !leaks(&rendering),
                "the legacy password escaped: {rendering}"
            );
        }

        // Absent, not redacted. A `"password": "[redacted]"` in the blob would
        // be written back over the pre-split credential and then handed to
        // `load_credentials` as the fallback password.
        assert!(
            as_value.get("password").is_none(),
            "the legacy password key was written back: {as_string}"
        );
        // The rest of the blob still round-trips.
        assert_eq!(as_value["host"], "smtp.acme.test");
        assert_eq!(as_value["username"], "mailer");
        assert_eq!(as_value["from_email"], "ceo@acme.test");
    }
}
