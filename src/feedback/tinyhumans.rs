//! Forwarding feedback to the TinyHumans hub behind a mockable client.
//!
//! A provisioned instance — one configured with a TinyHumans credential — sends
//! its feedback to the backend enrichment hub instead of filing a GitHub issue
//! itself, so the report is recorded on behalf of the credential's owner and
//! the hub decides whether an issue is ultimately filed.
//!
//! The [`TinyHumansClient`] trait and its offline [`MockTinyHumansClient`]
//! compile in the default build so the whole routing decision is exercised
//! without linking a network crate. Only the real HTTP client
//! [`HttpTinyHumansClient`] is gated behind the `tinyhumans` feature.
//!
//! Two invariants this module must not break:
//!
//! * **Only scrubbed text leaves.** The body handed to [`TinyHumansClient::ingest`]
//!   is the same one the scrub-then-preview gate produced, byte for byte, so
//!   forwarding is not a second path around [`crate::feedback::scrub`].
//! * **The credential never appears in a body.** It travels only as the
//!   `Authorization` header, held by the HTTP client and never by a request.

use std::sync::Mutex as StdMutex;

use async_trait::async_trait;

use crate::Result;
use crate::feedback::types::FeedbackCategory;

/// The product discriminator the hub routes on. Feedback from this runtime is
/// always attributed to opencompany, whichever company reported it.
pub const PRODUCT: &str = "opencompany";

/// One feedback report to forward.
///
/// `title` and `body` are the byte-exact strings the preview showed; `origin`
/// and `external_ref` let an operator trace a hub item back to the local one
/// without carrying anything private across.
#[derive(Clone, Debug, PartialEq)]
pub struct IngestRequest {
    /// The reported category, mapped to the hub's coarser type on the wire.
    pub category: FeedbackCategory,
    /// The issue title.
    pub title: String,
    /// The scrubbed, signed body.
    pub body: String,
    /// The reporting company's `@handle`.
    pub origin: String,
    /// The local [`FeedbackItem`](crate::feedback::FeedbackItem) id.
    pub external_ref: String,
}

impl IngestRequest {
    /// The hub's `type` for this report.
    ///
    /// The hub accepts only `feature | bug`, a coarser split than the local
    /// taxonomy. Nothing is lost: the precise category is the first line of
    /// every body (`**Category:** …`), and it also travels as `external_ref`'s
    /// companion in the local store.
    pub fn wire_type(&self) -> &'static str {
        match self.category {
            // Something the product did wrong.
            FeedbackCategory::Bug | FeedbackCategory::WrongOutput => "bug",
            // Something the product does not do (well enough) yet.
            FeedbackCategory::MissingCapability
            | FeedbackCategory::TemplateGap
            | FeedbackCategory::ApprovalFriction
            | FeedbackCategory::Docs => "feature",
        }
    }
}

/// What the hub did with a forwarded report.
#[derive(Clone, Debug, PartialEq)]
pub enum IngestOutcome {
    /// The hub accepted the report into the enrichment pipeline.
    Accepted {
        /// The hub's id for the report, when it returned one.
        remote_id: Option<String>,
    },
    /// The hub received the report but its moderation rejected it.
    Rejected {
        /// The human-safe moderation reason.
        reason: String,
    },
    /// The owner's daily feedback limit was reached; nothing was recorded.
    RateLimited {
        /// The human-safe limit message.
        reason: String,
    },
}

/// The TinyHumans backend, scoped to feedback ingestion.
#[async_trait]
pub trait TinyHumansClient: Send + Sync {
    /// Forwards one report to the hub, recorded as the credential's owner.
    async fn ingest(&self, request: &IngestRequest) -> Result<IngestOutcome>;
}

/// An in-memory [`TinyHumansClient`] for offline tests.
///
/// Every forwarded request is recorded so a test can assert the *scrubbed* body
/// crossed the boundary. Seed a non-accepting outcome with
/// [`with_outcome`](Self::with_outcome) or a transport failure with
/// [`with_failure`](Self::with_failure).
#[derive(Debug)]
pub struct MockTinyHumansClient {
    forwarded: StdMutex<Vec<IngestRequest>>,
    outcome: IngestOutcome,
    failure: Option<String>,
}

impl Default for MockTinyHumansClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTinyHumansClient {
    /// A mock that accepts everything.
    pub fn new() -> Self {
        Self {
            forwarded: StdMutex::new(Vec::new()),
            outcome: IngestOutcome::Accepted {
                remote_id: Some("hub-1".to_string()),
            },
            failure: None,
        }
    }

    /// Returns `outcome` instead of accepting.
    pub fn with_outcome(mut self, outcome: IngestOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Fails every forward with `message`, simulating an unreachable hub.
    pub fn with_failure(mut self, message: &str) -> Self {
        self.failure = Some(message.to_string());
        self
    }

    /// A snapshot of every request forwarded through this mock.
    pub fn forwarded(&self) -> Vec<IngestRequest> {
        self.forwarded.lock().expect("mock poisoned").clone()
    }
}

#[async_trait]
impl TinyHumansClient for MockTinyHumansClient {
    async fn ingest(&self, request: &IngestRequest) -> Result<IngestOutcome> {
        // Record before failing: a test asserting "we tried to send X" still
        // sees the attempt on the failure path.
        self.forwarded
            .lock()
            .expect("mock poisoned")
            .push(request.clone());
        if let Some(message) = &self.failure {
            return Err(crate::error::OpenCompanyError::TinyHumans {
                code: "unreachable".to_string(),
                message: message.clone(),
            });
        }
        Ok(self.outcome.clone())
    }
}

/// The real HTTP TinyHumans client, compiled only under the `tinyhumans` feature.
#[cfg(feature = "tinyhumans")]
pub use http::HttpTinyHumansClient;

#[cfg(feature = "tinyhumans")]
mod http {
    use super::{IngestOutcome, IngestRequest, PRODUCT, TinyHumansClient};
    use crate::Result;
    use crate::error::OpenCompanyError;
    use crate::ports::types::SecretValue;
    use async_trait::async_trait;

    /// A [`TinyHumansClient`] backed by `POST {api_url}/feedback/ingest`.
    ///
    /// The credential authenticates the call; the backend resolves it to the
    /// owning account, which is what makes a forwarded report "recorded on
    /// behalf of the key owner".
    pub struct HttpTinyHumansClient {
        api_url: String,
        credential: SecretValue,
        http: reqwest::Client,
    }

    impl HttpTinyHumansClient {
        /// Builds a client posting to `api_url` as `credential`'s owner.
        pub fn new(api_url: impl Into<String>, credential: SecretValue) -> Self {
            Self {
                // Trailing slashes would produce `//feedback/ingest`.
                api_url: api_url.into().trim_end_matches('/').to_string(),
                credential,
                http: reqwest::Client::new(),
            }
        }

        fn err(context: &str, e: impl std::fmt::Display) -> OpenCompanyError {
            OpenCompanyError::TinyHumans {
                code: context.to_string(),
                message: e.to_string(),
            }
        }
    }

    #[async_trait]
    impl TinyHumansClient for HttpTinyHumansClient {
        async fn ingest(&self, request: &IngestRequest) -> Result<IngestOutcome> {
            let url = format!("{}/feedback/ingest", self.api_url);
            let body = serde_json::json!({
                "type": request.wire_type(),
                "title": request.title,
                "body": request.body,
                "product": PRODUCT,
                "origin": request.origin,
                "externalRef": request.external_ref,
            });
            let resp = self
                .http
                .post(&url)
                // The credential rides the header and only the header.
                .bearer_auth(self.credential.expose())
                .json(&body)
                .send()
                .await
                .map_err(|e| Self::err("unreachable", e))?;

            let status = resp.status();
            let value: serde_json::Value = resp.json().await.map_err(|e| Self::err("decode", e))?;

            // The daily-limit refusal is a normal outcome for a busy operator,
            // not a transport failure: report it rather than erroring.
            if status.as_u16() == 429 {
                return Ok(IngestOutcome::RateLimited {
                    reason: wire_error(&value)
                        .unwrap_or_else(|| "daily feedback limit reached".to_string()),
                });
            }
            if !status.is_success() {
                return Err(OpenCompanyError::TinyHumans {
                    code: format!("http_{}", status.as_u16()),
                    message: wire_error(&value).unwrap_or_else(|| status.to_string()),
                });
            }

            // { success, data: { accepted, reason, feedback } } — a 200 with
            // `accepted: false` is a moderation rejection, not an error.
            let data = value.get("data").unwrap_or(&serde_json::Value::Null);
            let accepted = data
                .get("accepted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !accepted {
                return Ok(IngestOutcome::Rejected {
                    reason: data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("rejected by moderation")
                        .to_string(),
                });
            }
            Ok(IngestOutcome::Accepted {
                remote_id: data
                    .get("feedback")
                    .and_then(|f| f.get("id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        }
    }

    /// The `error` string from a failure envelope, when present.
    fn wire_error(value: &serde_json::Value) -> Option<String> {
        value
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn request(category: FeedbackCategory) -> IngestRequest {
        IngestRequest {
            category,
            title: "[bug] it broke".to_string(),
            body: "**Category:** bug\n\nit broke\n\n— filed by @acme".to_string(),
            origin: "acme".to_string(),
            external_ref: "item-1".to_string(),
        }
    }

    #[test]
    fn maps_categories_onto_the_hub_type_pair() {
        // Something the product did wrong.
        for category in [FeedbackCategory::Bug, FeedbackCategory::WrongOutput] {
            assert_eq!(request(category).wire_type(), "bug", "{category:?}");
        }
        // Something the product does not do yet.
        for category in [
            FeedbackCategory::MissingCapability,
            FeedbackCategory::TemplateGap,
            FeedbackCategory::ApprovalFriction,
            FeedbackCategory::Docs,
        ] {
            assert_eq!(request(category).wire_type(), "feature", "{category:?}");
        }
    }

    #[test]
    fn product_is_always_opencompany() {
        assert_eq!(PRODUCT, "opencompany");
    }

    #[tokio::test]
    async fn mock_records_the_forwarded_request() {
        let client = MockTinyHumansClient::new();
        let outcome = client
            .ingest(&request(FeedbackCategory::Bug))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            IngestOutcome::Accepted {
                remote_id: Some("hub-1".to_string())
            }
        );
        let forwarded = client.forwarded();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].external_ref, "item-1");
        assert_eq!(forwarded[0].origin, "acme");
    }

    #[tokio::test]
    async fn mock_can_reject_and_fail() {
        let rejected = MockTinyHumansClient::new().with_outcome(IngestOutcome::Rejected {
            reason: "spam".to_string(),
        });
        assert_eq!(
            rejected
                .ingest(&request(FeedbackCategory::Bug))
                .await
                .unwrap(),
            IngestOutcome::Rejected {
                reason: "spam".to_string()
            }
        );

        let failing = MockTinyHumansClient::new().with_failure("connection refused");
        assert!(
            failing
                .ingest(&request(FeedbackCategory::Bug))
                .await
                .is_err()
        );
        // The attempt is still recorded, so a test can assert what we tried to send.
        assert_eq!(failing.forwarded().len(), 1);
    }
}
