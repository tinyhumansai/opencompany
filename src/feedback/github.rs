//! GitHub issue filing behind a mockable client.
//!
//! The [`GitHubClient`] trait and its offline [`MockGitHubClient`] compile in
//! the default build so the whole filing flow — dedupe, rate-limit, signing,
//! labels, and the tokenless manual-link fallback — is exercised offline. Only
//! the real HTTP filer [`HttpGitHubClient`] is gated behind the `github`
//! feature.
//!
//! Filing obligations (`docs/spec/feedback-loop/README.md`):
//!
//! * **Dedupe** — search existing issues first; comment instead of duplicating.
//! * **Rate-limit** — per company, so a noisy company cannot flood the tracker.
//! * **Sign** — the body carries the company `@handle` for provenance.
//! * **Degrade** — without a token, return a prefilled manual issue link.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;

use crate::Result;
use crate::feedback::types::ConsentMode;

/// A draft issue to create.
#[derive(Clone, Debug, PartialEq)]
pub struct IssueDraft {
    /// The issue title.
    pub title: String,
    /// The issue body (already scrubbed and signed).
    pub body: String,
    /// The issue labels.
    pub labels: Vec<String>,
}

/// A pre-existing issue found by a dedupe search.
#[derive(Clone, Debug, PartialEq)]
pub struct ExistingIssue {
    /// The issue number.
    pub number: u64,
    /// The issue URL.
    pub url: String,
    /// The issue title.
    pub title: String,
}

/// A GitHub REST client scoped to issue filing.
#[async_trait]
pub trait GitHubClient: Send + Sync {
    /// Searches a repo for issues matching `query` (dedupe).
    async fn search_issues(&self, repo: &str, query: &str) -> Result<Vec<ExistingIssue>>;
    /// Creates an issue, returning its URL.
    async fn create_issue(&self, repo: &str, draft: &IssueDraft) -> Result<String>;
    /// Comments on an existing issue (the dedupe path).
    async fn comment_issue(&self, repo: &str, number: u64, body: &str) -> Result<()>;
}

/// The result of a filing attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum FilingOutcome {
    /// A new issue was created at this URL.
    Filed {
        /// The created issue URL.
        url: String,
    },
    /// An existing issue matched; a comment was added instead of duplicating.
    Deduped {
        /// The existing issue URL.
        url: String,
    },
    /// The per-company rate limit was hit; nothing was filed.
    RateLimited,
    /// No token (or manual consent): the operator files via this prefilled link.
    ManualLink {
        /// A prefilled `issues/new` URL.
        url: String,
    },
}

/// A simple per-company token bucket, in memory.
///
/// Each company may file at most `max_per_company` issues per process lifetime;
/// exceeding that returns [`FilingOutcome::RateLimited`]. A real deployment
/// would persist and window this, but the in-memory bucket keeps a noisy
/// company from flooding the tracker within a run.
#[derive(Debug)]
pub struct RateLimiter {
    max_per_company: usize,
    used: StdMutex<HashMap<String, usize>>,
}

impl RateLimiter {
    /// Builds a limiter allowing `max_per_company` filings per company.
    pub fn new(max_per_company: usize) -> Self {
        Self {
            max_per_company,
            used: StdMutex::new(HashMap::new()),
        }
    }

    /// Records a filing for `company`, returning `true` if it is within budget.
    pub fn allow(&self, company: &str) -> bool {
        let mut used = self.used.lock().expect("rate limiter poisoned");
        let count = used.entry(company.to_string()).or_insert(0);
        if *count >= self.max_per_company {
            return false;
        }
        *count += 1;
        true
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(20)
    }
}

/// Files a feedback issue, honoring consent, dedupe, rate-limit, and the
/// tokenless fallback.
///
/// `body` must already be scrubbed and signed with the company `@handle`. When
/// `client` is `None` or `consent` is [`ConsentMode::Manual`], the operator
/// files via the returned [`FilingOutcome::ManualLink`]. Otherwise the client
/// searches for a duplicate (commenting if found), checks the per-company rate
/// limit, and creates the issue.
#[allow(clippy::too_many_arguments)]
pub async fn file_feedback(
    client: Option<&dyn GitHubClient>,
    repo: &str,
    company: &str,
    title: &str,
    body: &str,
    labels: &[String],
    consent: ConsentMode,
    limiter: &RateLimiter,
) -> Result<FilingOutcome> {
    // Manual consent or no client: degrade to a prefilled manual link.
    let Some(client) = client.filter(|_| consent != ConsentMode::Manual) else {
        return Ok(FilingOutcome::ManualLink {
            url: manual_issue_url(repo, title, body, labels),
        });
    };

    // Dedupe: comment on an existing issue instead of duplicating.
    let existing = client.search_issues(repo, title).await?;
    if let Some(issue) = existing.into_iter().next() {
        client.comment_issue(repo, issue.number, body).await?;
        return Ok(FilingOutcome::Deduped { url: issue.url });
    }

    // Rate-limit per company.
    if !limiter.allow(company) {
        return Ok(FilingOutcome::RateLimited);
    }

    let url = client
        .create_issue(
            repo,
            &IssueDraft {
                title: title.to_string(),
                body: body.to_string(),
                labels: labels.to_vec(),
            },
        )
        .await?;
    Ok(FilingOutcome::Filed { url })
}

/// Appends the company `@handle` provenance signature to an issue body.
pub fn sign_body(body: &str, handle: &str) -> String {
    format!("{body}\n\n— filed by @{handle}")
}

/// Builds a prefilled `issues/new` URL with a url-encoded title, body, labels.
pub fn manual_issue_url(repo: &str, title: &str, body: &str, labels: &[String]) -> String {
    let mut url = format!("https://github.com/{repo}/issues/new");
    url.push_str("?title=");
    url.push_str(&percent_encode(title));
    url.push_str("&body=");
    url.push_str(&percent_encode(body));
    if !labels.is_empty() {
        url.push_str("&labels=");
        url.push_str(&percent_encode(&labels.join(",")));
    }
    url
}

/// Percent-encodes a string for a URL query value (RFC 3986 unreserved set).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// An in-memory [`GitHubClient`] for offline tests.
///
/// Seed canned dedupe hits with [`with_existing`](Self::with_existing); every
/// `create_issue` and `comment_issue` is recorded so a test can assert dedupe
/// commented rather than duplicated.
#[derive(Debug, Default)]
pub struct MockGitHubClient {
    existing: StdMutex<Vec<ExistingIssue>>,
    created: StdMutex<Vec<IssueDraft>>,
    comments: StdMutex<Vec<(u64, String)>>,
    next_number: StdMutex<u64>,
}

impl MockGitHubClient {
    /// A mock with no existing issues.
    pub fn new() -> Self {
        Self {
            existing: StdMutex::new(Vec::new()),
            created: StdMutex::new(Vec::new()),
            comments: StdMutex::new(Vec::new()),
            next_number: StdMutex::new(100),
        }
    }

    /// Seeds an existing issue that dedupe searches will return.
    pub fn with_existing(self, number: u64, url: &str, title: &str) -> Self {
        self.existing
            .lock()
            .expect("mock poisoned")
            .push(ExistingIssue {
                number,
                url: url.to_string(),
                title: title.to_string(),
            });
        self
    }

    /// A snapshot of every issue created through this mock.
    pub fn created(&self) -> Vec<IssueDraft> {
        self.created.lock().expect("mock poisoned").clone()
    }

    /// A snapshot of every comment `(issue_number, body)` recorded.
    pub fn comments(&self) -> Vec<(u64, String)> {
        self.comments.lock().expect("mock poisoned").clone()
    }
}

#[async_trait]
impl GitHubClient for MockGitHubClient {
    async fn search_issues(&self, _repo: &str, query: &str) -> Result<Vec<ExistingIssue>> {
        // A naive contains-match on the title, enough to exercise dedupe.
        let hits = self
            .existing
            .lock()
            .expect("mock poisoned")
            .iter()
            .filter(|issue| query.contains(&issue.title) || issue.title.contains(query))
            .cloned()
            .collect();
        Ok(hits)
    }

    async fn create_issue(&self, _repo: &str, draft: &IssueDraft) -> Result<String> {
        self.created
            .lock()
            .expect("mock poisoned")
            .push(draft.clone());
        let number = {
            let mut number = self.next_number.lock().expect("mock poisoned");
            let current = *number;
            *number += 1;
            current
        };
        let url = format!("https://github.com/mock/issues/{number}");
        // Register the new issue so subsequent dedupe searches find it, exactly
        // as the real tracker would.
        self.existing
            .lock()
            .expect("mock poisoned")
            .push(ExistingIssue {
                number,
                url: url.clone(),
                title: draft.title.clone(),
            });
        Ok(url)
    }

    async fn comment_issue(&self, _repo: &str, number: u64, body: &str) -> Result<()> {
        self.comments
            .lock()
            .expect("mock poisoned")
            .push((number, body.to_string()));
        Ok(())
    }
}

/// The real HTTP GitHub client, compiled only under the `github` feature.
#[cfg(feature = "github")]
pub use http::HttpGitHubClient;

#[cfg(feature = "github")]
mod http {
    use super::{ExistingIssue, GitHubClient, IssueDraft};
    use crate::Result;
    use crate::error::OpenCompanyError;
    use crate::ports::types::SecretValue;
    use async_trait::async_trait;

    /// A [`GitHubClient`] backed by the GitHub REST API and a `GITHUB_TOKEN`.
    pub struct HttpGitHubClient {
        token: SecretValue,
        http: reqwest::Client,
    }

    impl HttpGitHubClient {
        /// Builds a client authenticating with `token`.
        pub fn new(token: SecretValue) -> Self {
            Self {
                token,
                http: reqwest::Client::new(),
            }
        }

        fn err(context: &str, e: impl std::fmt::Display) -> OpenCompanyError {
            OpenCompanyError::OpenHuman {
                code: -1,
                message: format!("github {context}: {e}"),
            }
        }
    }

    #[async_trait]
    impl GitHubClient for HttpGitHubClient {
        async fn search_issues(&self, repo: &str, query: &str) -> Result<Vec<ExistingIssue>> {
            let url = "https://api.github.com/search/issues";
            let q = format!("repo:{repo} in:title {query}");
            let resp = self
                .http
                .get(url)
                .query(&[("q", q.as_str())])
                .header("Authorization", format!("Bearer {}", self.token.expose()))
                .header("User-Agent", "opencompany")
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|e| Self::err("search", e))?;
            let value: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| Self::err("search-decode", e))?;
            let mut out = Vec::new();
            if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
                for item in items {
                    let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
                    let url = item
                        .get("html_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let title = item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    out.push(ExistingIssue { number, url, title });
                }
            }
            Ok(out)
        }

        async fn create_issue(&self, repo: &str, draft: &IssueDraft) -> Result<String> {
            let url = format!("https://api.github.com/repos/{repo}/issues");
            let resp = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.token.expose()))
                .header("User-Agent", "opencompany")
                .header("Accept", "application/vnd.github+json")
                .json(&serde_json::json!({
                    "title": draft.title,
                    "body": draft.body,
                    "labels": draft.labels,
                }))
                .send()
                .await
                .map_err(|e| Self::err("create", e))?;
            let value: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| Self::err("create-decode", e))?;
            Ok(value
                .get("html_url")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string())
        }

        async fn comment_issue(&self, repo: &str, number: u64, body: &str) -> Result<()> {
            let url = format!("https://api.github.com/repos/{repo}/issues/{number}/comments");
            self.http
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.token.expose()))
                .header("User-Agent", "opencompany")
                .header("Accept", "application/vnd.github+json")
                .json(&serde_json::json!({ "body": body }))
                .send()
                .await
                .map_err(|e| Self::err("comment", e))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn no_client_degrades_to_manual_link() {
        let limiter = RateLimiter::default();
        let out = file_feedback(
            None,
            "tinyhumansai/opencompany",
            "acme",
            "[bug] broken route",
            "body — filed by @acme",
            &["feedback".into(), "source/agent-filed".into()],
            ConsentMode::Auto,
            &limiter,
        )
        .await
        .unwrap();
        match out {
            FilingOutcome::ManualLink { url } => {
                assert!(url.starts_with("https://github.com/tinyhumansai/opencompany/issues/new"));
                assert!(url.contains("title="));
                assert!(url.contains("body="));
                assert!(url.contains("labels="));
            }
            other => panic!("expected ManualLink, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manual_consent_never_calls_client() {
        let client = MockGitHubClient::new();
        let limiter = RateLimiter::default();
        let out = file_feedback(
            Some(&client),
            "r/r",
            "acme",
            "t",
            "b",
            &[],
            ConsentMode::Manual,
            &limiter,
        )
        .await
        .unwrap();
        assert!(matches!(out, FilingOutcome::ManualLink { .. }));
        assert!(client.created().is_empty());
    }

    #[tokio::test]
    async fn auto_consent_creates_issue_with_labels_and_signature() {
        let client = MockGitHubClient::new();
        let limiter = RateLimiter::default();
        let body = sign_body("the route is broken", "acme");
        let labels = vec![
            "feedback".to_string(),
            "type/bug".to_string(),
            "area/runtime".to_string(),
            "sev/annoyance".to_string(),
            "source/agent-filed".to_string(),
        ];
        let out = file_feedback(
            Some(&client),
            "r/r",
            "acme",
            "[bug] route",
            &body,
            &labels,
            ConsentMode::Auto,
            &limiter,
        )
        .await
        .unwrap();
        assert!(matches!(out, FilingOutcome::Filed { .. }));

        let created = client.created();
        assert_eq!(created.len(), 1);
        assert!(created[0].body.contains("— filed by @acme"));
        assert!(
            created[0]
                .labels
                .contains(&"source/agent-filed".to_string())
        );
        assert!(created[0].labels.contains(&"type/bug".to_string()));
    }

    #[tokio::test]
    async fn dedupe_comments_instead_of_creating() {
        let client = MockGitHubClient::new().with_existing(
            42,
            "https://github.com/mock/issues/42",
            "[bug] route",
        );
        let limiter = RateLimiter::default();
        let out = file_feedback(
            Some(&client),
            "r/r",
            "acme",
            "[bug] route",
            "body — filed by @acme",
            &[],
            ConsentMode::Auto,
            &limiter,
        )
        .await
        .unwrap();
        match out {
            FilingOutcome::Deduped { url } => assert_eq!(url, "https://github.com/mock/issues/42"),
            other => panic!("expected Deduped, got {other:?}"),
        }
        assert!(client.created().is_empty());
        assert_eq!(client.comments().len(), 1);
        assert_eq!(client.comments()[0].0, 42);
    }

    #[tokio::test]
    async fn rate_limit_trips_after_budget() {
        let client = MockGitHubClient::new();
        let limiter = RateLimiter::new(1);
        let file = |title: &'static str| {
            file_feedback(
                Some(&client),
                "r/r",
                "acme",
                title,
                "b — filed by @acme",
                &[],
                ConsentMode::Auto,
                &limiter,
            )
        };
        assert!(matches!(
            file("one").await.unwrap(),
            FilingOutcome::Filed { .. }
        ));
        assert!(matches!(
            file("two").await.unwrap(),
            FilingOutcome::RateLimited
        ));
    }

    #[test]
    fn signature_carries_handle() {
        assert_eq!(sign_body("x", "acme"), "x\n\n— filed by @acme");
    }
}
