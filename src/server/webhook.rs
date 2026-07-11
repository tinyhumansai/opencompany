//! Outbound platform webhooks: an at-least-once delivery seam behind a
//! mockable [`WebhookSink`] trait.
//!
//! The default build ships an in-memory [`RecordingWebhookSink`] and a
//! deterministic, non-cryptographic signer ([`DefaultHashSigner`]) so the whole
//! surface is exercised offline by `cargo test`. Real HTTP POST with an
//! HMAC-SHA256 signature is added under the `webhooks` feature (via
//! `HttpWebhookSink` and `HmacSha256Signer`); nothing here links a network or
//! crypto crate in the default build.
//!
//! Every delivery carries an `X-OpenCompany-Signature`-equivalent header value
//! computed by the configured [`WebhookSigner`] over the JSON body: `kh1=<hex>`
//! by default, `sha256=<hex>` under `webhooks`.

use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::Result;
use crate::ports::types::CompanyId;

/// The category of a platform webhook (api.md §Platform webhooks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookKind {
    /// An effect was parked and awaits operator approval.
    ApprovalRequested,
    /// A company completed a unit of work (a cycle produced output).
    WorkCompleted,
    /// A feedback item was captured.
    FeedbackCreated,
    /// A company's budget was exhausted.
    BudgetExhausted,
}

impl WebhookKind {
    /// The stable wire string for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApprovalRequested => "approval_requested",
            Self::WorkCompleted => "work_completed",
            Self::FeedbackCreated => "feedback_created",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// A platform webhook event delivered to the tenant's configured sink.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WebhookEvent {
    /// The event category.
    #[serde(rename = "type")]
    pub kind: WebhookKind,
    /// The company the event is about.
    pub company_id: CompanyId,
    /// Epoch-millis timestamp the event was produced.
    pub at_millis: u64,
    /// Event-specific payload.
    pub data: serde_json::Value,
}

impl WebhookEvent {
    /// Builds an event stamped with the current time.
    pub fn now(kind: WebhookKind, company_id: CompanyId, data: serde_json::Value) -> Self {
        Self {
            kind,
            company_id,
            at_millis: crate::ports::now_millis(),
            data,
        }
    }
}

/// The at-least-once delivery seam. The default build uses the in-memory
/// [`RecordingWebhookSink`]; real HTTP POST is added under `webhooks`.
#[async_trait::async_trait]
pub trait WebhookSink: Send + Sync {
    /// Delivers `event` with the precomputed `signature` header value. An
    /// implementation may retry internally; a returned error means every attempt
    /// failed (and the caller logs, never blocking the cycle).
    async fn deliver(&self, event: &WebhookEvent, signature: &str) -> Result<()>;
}

/// An offline mock sink that records every delivery for assertions and never
/// fails. Used by the default build and tests.
#[derive(Clone, Default)]
pub struct RecordingWebhookSink {
    sent: Arc<Mutex<Vec<(WebhookEvent, String)>>>,
}

impl RecordingWebhookSink {
    /// Creates an empty recording sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of every `(event, signature)` delivered so far.
    pub fn delivered(&self) -> Vec<(WebhookEvent, String)> {
        self.sent.lock().expect("webhook sink poisoned").clone()
    }

    /// The number of deliveries recorded.
    pub fn count(&self) -> usize {
        self.sent.lock().expect("webhook sink poisoned").len()
    }
}

#[async_trait::async_trait]
impl WebhookSink for RecordingWebhookSink {
    async fn deliver(&self, event: &WebhookEvent, signature: &str) -> Result<()> {
        self.sent
            .lock()
            .expect("webhook sink poisoned")
            .push((event.clone(), signature.to_string()));
        Ok(())
    }
}

/// The signing seam so the delivery header is deterministic and offline-testable.
pub trait WebhookSigner: Send + Sync {
    /// Signs `body` with the tenant `secret`, returning the header value.
    fn sign(&self, secret: &str, body: &[u8]) -> String;
}

/// The default-build signer: a deterministic, non-cryptographic keyed hash over
/// `secret ‖ body`, rendered `kh1=<hex>`. Clearly labelled insecure; real
/// HMAC-SHA256 lives behind the `webhooks` feature.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultHashSigner;

impl WebhookSigner for DefaultHashSigner {
    fn sign(&self, secret: &str, body: &[u8]) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        secret.as_bytes().hash(&mut hasher);
        body.hash(&mut hasher);
        format!("kh1={:016x}", hasher.finish())
    }
}

/// A tenant's webhook delivery configuration: the sink, the signer, and the
/// per-tenant signing secret.
#[derive(Clone)]
pub struct WebhookConfig {
    /// The delivery sink (recording mock by default; HTTP under `webhooks`).
    pub sink: Arc<dyn WebhookSink>,
    /// The signer that computes the delivery header value.
    pub signer: Arc<dyn WebhookSigner>,
    /// The shared secret mixed into the signature.
    pub secret: String,
}

impl WebhookConfig {
    /// Builds a config over the in-memory recording sink and the deterministic
    /// signer — the offline default used by the CLI when no real transport is
    /// linked, and by tests.
    pub fn recording(secret: impl Into<String>) -> (Self, RecordingWebhookSink) {
        let sink = RecordingWebhookSink::new();
        (
            Self {
                sink: Arc::new(sink.clone()),
                signer: Arc::new(DefaultHashSigner),
                secret: secret.into(),
            },
            sink,
        )
    }

    /// Signs and delivers `event` with bounded best-effort retry. A delivery
    /// failure is logged and swallowed so a webhook never blocks a cycle.
    pub async fn emit(&self, event: &WebhookEvent) {
        let body = match serde_json::to_vec(event) {
            Ok(body) => body,
            Err(err) => {
                tracing::warn!(company = %event.company_id, "webhook serialize failed: {err}");
                return;
            }
        };
        let signature = self.signer.sign(&self.secret, &body);
        // At-least-once: a few bounded attempts, then give up with a warning.
        for attempt in 1..=3u32 {
            match self.sink.deliver(event, &signature).await {
                Ok(()) => return,
                Err(err) if attempt == 3 => {
                    tracing::warn!(
                        company = %event.company_id,
                        kind = event.kind.as_str(),
                        "webhook delivery failed after {attempt} attempts: {err}"
                    );
                }
                Err(_) => continue,
            }
        }
    }
}

impl std::fmt::Debug for WebhookConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookConfig")
            .field("secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Real HMAC-SHA256 signer, emitting `sha256=<hex>`. Gated so the default build
/// links no crypto.
#[cfg(feature = "webhooks")]
#[derive(Clone, Copy, Debug, Default)]
pub struct HmacSha256Signer;

#[cfg(feature = "webhooks")]
impl WebhookSigner for HmacSha256Signer {
    fn sign(&self, secret: &str, body: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(bytes.len() * 2 + 7);
        hex.push_str("sha256=");
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

/// Real HTTP POST sink. Delivers the event as JSON with the signature header,
/// retrying a bounded number of times. Gated behind `webhooks`.
#[cfg(feature = "webhooks")]
pub struct HttpWebhookSink {
    url: String,
    client: reqwest::Client,
}

#[cfg(feature = "webhooks")]
impl HttpWebhookSink {
    /// The header carrying the delivery signature.
    pub const SIGNATURE_HEADER: &'static str = "X-OpenCompany-Signature";

    /// Builds a sink posting to `url`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[cfg(feature = "webhooks")]
#[async_trait::async_trait]
impl WebhookSink for HttpWebhookSink {
    async fn deliver(&self, event: &WebhookEvent, signature: &str) -> Result<()> {
        let resp = self
            .client
            .post(&self.url)
            .header(Self::SIGNATURE_HEADER, signature)
            .json(event)
            .send()
            .await
            .map_err(|e| {
                crate::error::OpenCompanyError::Store(format!("webhook POST failed: {e}"))
            })?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(crate::error::OpenCompanyError::Store(format!(
                "webhook endpoint returned {}",
                resp.status()
            )))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn default_signer_is_deterministic_and_prefixed() {
        let signer = DefaultHashSigner;
        let a = signer.sign("secret", b"body");
        let b = signer.sign("secret", b"body");
        assert_eq!(a, b);
        assert!(a.starts_with("kh1="));
        // A different secret or body changes the signature.
        assert_ne!(a, signer.sign("other", b"body"));
        assert_ne!(a, signer.sign("secret", b"other"));
    }

    #[tokio::test]
    async fn recording_sink_captures_deliveries() {
        let (config, sink) = WebhookConfig::recording("s3cret");
        let event = WebhookEvent::now(
            WebhookKind::ApprovalRequested,
            CompanyId::new("acme"),
            serde_json::json!({ "approval_id": "a1" }),
        );
        config.emit(&event).await;

        let delivered = sink.delivered();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0.kind, WebhookKind::ApprovalRequested);
        assert!(delivered[0].1.starts_with("kh1="));
    }

    #[test]
    fn webhook_event_serializes_type_and_snake_case_kind() {
        let event = WebhookEvent::now(
            WebhookKind::WorkCompleted,
            CompanyId::new("acme"),
            serde_json::Value::Null,
        );
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "work_completed");
        assert_eq!(json["company_id"], "acme");
    }
}
