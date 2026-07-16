//! SMTP credentials + test send.
//!
//! `PUT …/smtp` stores credentials in [`SecretStore`](crate::ports::SecretStore)
//! under [`SMTP_KEY`](super::SMTP_KEY) and returns a non-secret
//! [`SmtpStatus`] — the password never appears in any response. `POST …/smtp/test`
//! sends a test email through the mockable [`MailSender`] seam, pulling the
//! stored credentials per send, and records the sent mail in the company's
//! [`InboxStore`](crate::ports::InboxStore) so the console shows it. The real
//! `lettre` transport is gated behind the `smtp` feature; without an injected
//! sender the test route is "not wired yet" (404).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::error::OpenCompanyError;
use crate::ports::inbox::EmailRecord;
use crate::ports::types::SecretValue;
use crate::ports::{generate_id, now_millis};
use crate::server::error::ApiError;
use crate::server::operator::OperatorAuth;
use crate::server::ops::{SMTP_KEY, resolve, resolve_sole};

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
/// response (the password would leak).
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
    pub password: String,
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

/// The outbound-send seam. Mockable so the test route is exercised offline; the
/// real `lettre` transport is gated behind the `smtp` feature.
#[async_trait]
pub trait MailSender: Send + Sync {
    /// Sends `email` using `creds`. An error means the message was not accepted.
    async fn send(
        &self,
        creds: &SmtpCredentials,
        email: &OutboundEmail,
    ) -> Result<(), OpenCompanyError>;
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
        creds: &SmtpCredentials,
        email: &OutboundEmail,
    ) -> Result<(), OpenCompanyError> {
        self.sent
            .lock()
            .expect("mail sender poisoned")
            .push((creds.from_email.clone(), email.clone()));
        Ok(())
    }
}

/// Builds the SMTP route fragment.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/companies/{id}/smtp", put(put_smtp))
        .route("/api/v1/companies/{id}/smtp/test", post(test_smtp))
        .route("/api/v1/company/smtp", put(put_smtp_single))
        .route("/api/v1/company/smtp/test", post(test_smtp_single))
}

// -- PUT smtp ---------------------------------------------------------------

/// Persists credentials and returns the non-secret status.
async fn store_credentials(
    runtime: Arc<CompanyRuntime>,
    creds: SmtpCredentials,
) -> Result<Json<SmtpStatus>, ApiError> {
    let json = serde_json::to_string(&creds)?;
    runtime
        .secrets()
        .set(runtime.id(), SMTP_KEY, SecretValue(json))
        .await?;
    Ok(Json(SmtpStatus::from_credentials(&creds)))
}

/// `PUT /api/v1/companies/{id}/smtp`.
async fn put_smtp(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(creds): Json<SmtpCredentials>,
) -> Result<Json<SmtpStatus>, ApiError> {
    store_credentials(resolve(&state, &id)?, creds).await
}

/// `PUT /api/v1/company/smtp` (single-company alias).
async fn put_smtp_single(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Json(creds): Json<SmtpCredentials>,
) -> Result<Json<SmtpStatus>, ApiError> {
    store_credentials(resolve_sole(&state)?, creds).await
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
) -> Result<Json<TestResult>, Response> {
    use axum::response::IntoResponse;
    // Not wired without a sender (default build / no `smtp` feature).
    let Some(sender) = state.connections().mail.clone() else {
        return Err(super::not_wired("smtp test send"));
    };
    let creds = load_credentials(&runtime)
        .await
        .map_err(|e| ApiError(e).into_response())?;
    let Some(creds) = creds else {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            "no SMTP credentials configured".to_string(),
        ))
        .into_response());
    };
    let to = body.to.unwrap_or_else(|| creds.from_email.clone());
    let email = OutboundEmail {
        to: to.clone(),
        subject: "OpenCompany SMTP test".to_string(),
        body: "This is a test message confirming your outbound email is wired up.".to_string(),
    };
    match sender.send(&creds, &email).await {
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

/// Loads and parses stored SMTP credentials, if any.
pub(crate) async fn load_credentials(
    runtime: &CompanyRuntime,
) -> Result<Option<SmtpCredentials>, OpenCompanyError> {
    let Some(value) = runtime.secrets().get(runtime.id(), SMTP_KEY).await? else {
        return Ok(None);
    };
    let creds: SmtpCredentials = serde_json::from_str(value.expose())?;
    Ok(Some(creds))
}

/// Appends a sent email to the sender's inbox so the console shows outbound mail.
async fn record_outbound(runtime: &CompanyRuntime, creds: &SmtpCredentials, email: &OutboundEmail) {
    let record = EmailRecord {
        id: generate_id(),
        inbox: local_part(&creds.from_email),
        from: creds.from_email.clone(),
        to: email.to.clone(),
        subject: email.subject.clone(),
        body: email.body.clone(),
        outbound: true,
        at_millis: now_millis(),
    };
    if let Err(err) = runtime.inbox().append(runtime.id(), record).await {
        tracing::warn!(company = %runtime.id(), "failed to record outbound email: {err}");
    }
}

/// The local part of an address (`ceo@acme.test` → `ceo`), or the whole string
/// when it carries no `@`.
pub(crate) fn local_part(address: &str) -> String {
    address
        .split_once('@')
        .map(|(local, _)| local.to_string())
        .unwrap_or_else(|| address.to_string())
}

/// `POST /api/v1/companies/{id}/smtp/test`.
async fn test_smtp(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<TestSend>>,
) -> Result<Json<TestResult>, Response> {
    use axum::response::IntoResponse;
    let runtime = resolve(&state, &id).map_err(IntoResponse::into_response)?;
    run_test(&state, runtime, body.map(|b| b.0).unwrap_or_default()).await
}

/// `POST /api/v1/company/smtp/test` (single-company alias).
async fn test_smtp_single(
    _auth: OperatorAuth,
    State(state): State<AppState>,
    body: Option<Json<TestSend>>,
) -> Result<Json<TestResult>, Response> {
    use axum::response::IntoResponse;
    let runtime = resolve_sole(&state).map_err(IntoResponse::into_response)?;
    run_test(&state, runtime, body.map(|b| b.0).unwrap_or_default()).await
}

/// The real `lettre` SMTP transport. Gated behind the `smtp` feature so the
/// default build links no SMTP crate.
#[cfg(feature = "smtp")]
pub struct LettreMailSender;

#[cfg(feature = "smtp")]
#[async_trait]
impl MailSender for LettreMailSender {
    async fn send(
        &self,
        creds: &SmtpCredentials,
        email: &OutboundEmail,
    ) -> Result<(), OpenCompanyError> {
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

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

        let builder = match creds.security {
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
        let transport = builder
            .credentials(Credentials::new(
                creds.username.clone(),
                creds.password.clone(),
            ))
            .build();
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
            password: "s3cret-pw".into(),
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
        let sender = RecordingMailSender::new();
        let creds = SmtpCredentials {
            host: "h".into(),
            port: 25,
            security: SmtpSecurity::None,
            username: "u".into(),
            password: "p".into(),
            from_name: String::new(),
            from_email: "from@x.test".into(),
        };
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
