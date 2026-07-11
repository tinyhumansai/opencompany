//! The feedback filing flow: capture → scrub → preview → file.
//!
//! [`finalize`] runs the scrub-then-preview gate over an already-captured
//! [`FeedbackItem`] and either previews the exact final body or files it. It is
//! driven by the HTTP `POST .../feedback` handler through
//! [`CompanyRuntime::submit_feedback`](crate::company::runtime::CompanyRuntime::submit_feedback).

use std::sync::Arc;

use serde::Serialize;

use crate::Result;
use crate::company::CompanyManifest;
use crate::feedback::github::{
    FilingOutcome, GitHubClient, RateLimiter, file_feedback, manual_issue_url,
};
use crate::feedback::scrub::{CharterTerm, ScrubOutcome, scrub};
use crate::feedback::store::FeedbackStore;
use crate::feedback::triage::{FeedbackSource, QualityLedger, Severity, classify_labels};
use crate::feedback::types::{ConsentMode, FeedbackItem};
use crate::ports::SecretStore;
use crate::ports::types::CompanyId;

/// The per-company filing configuration: the GitHub client (if any), the target
/// repo, the standing consent mode, and the rate limiter.
pub struct FeedbackFiler {
    /// The GitHub client, or `None` to always degrade to a manual link.
    pub client: Option<Arc<dyn GitHubClient>>,
    /// The `owner/repo` issues are filed against.
    pub repo: String,
    /// The standing consent mode.
    pub consent: ConsentMode,
    /// The per-company rate limiter.
    pub limiter: RateLimiter,
    /// Per-handle filing quality, throttling auto-consent after repeated
    /// low-quality filings.
    pub quality: QualityLedger,
}

impl Default for FeedbackFiler {
    fn default() -> Self {
        Self {
            client: None,
            repo: crate::feedback::DEFAULT_REPO.to_string(),
            consent: ConsentMode::default(),
            limiter: RateLimiter::default(),
            quality: QualityLedger::default(),
        }
    }
}

impl std::fmt::Debug for FeedbackFiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedbackFiler")
            .field("repo", &self.repo)
            .field("consent", &self.consent)
            .field("has_client", &self.client.is_some())
            .finish_non_exhaustive()
    }
}

/// The response body for a feedback submission, mirroring the api.md envelope.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FeedbackResponse {
    /// The captured item's id (it persists regardless of filing).
    pub item_id: String,
    /// Whether an issue was filed (created or commented).
    pub filed: bool,
    /// Whether filing was blocked by the scrubber (fail-closed).
    pub blocked: bool,
    /// A human-safe reason when blocked or rate-limited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The exact final issue body, returned in preview mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_body: Option<String>,
    /// A prefilled manual issue link (preview, or the tokenless fallback).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefilled_url: Option<String>,
    /// The filed issue URL, when filed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_url: Option<String>,
    /// Whether the filing commented on an existing issue (dedupe).
    pub deduped: bool,
}

impl FeedbackResponse {
    fn blocked(item_id: &str, reason: String) -> Self {
        Self {
            item_id: item_id.to_string(),
            filed: false,
            blocked: true,
            reason: Some(reason),
            preview_body: None,
            prefilled_url: None,
            issue_url: None,
            deduped: false,
        }
    }
}

/// Runs the scrub-then-preview gate for `item` and either previews or files.
///
/// * A scrub abort → a `blocked` response that never leaks the offending value.
/// * `preview` → the byte-exact final body plus a prefilled link.
/// * Otherwise → file through the [`FeedbackFiler`], updating the stored item's
///   status on success.
#[allow(clippy::too_many_arguments)]
pub async fn finalize(
    store: &FeedbackStore,
    secrets: &dyn SecretStore,
    filer: &FeedbackFiler,
    company: &CompanyId,
    manifest: Option<&CompanyManifest>,
    item: &FeedbackItem,
    severity: Severity,
    source: FeedbackSource,
    preview: bool,
) -> Result<FeedbackResponse> {
    let handle = manifest
        .and_then(|m| m.company.handle.clone())
        .unwrap_or_else(|| company.as_ref().to_string());
    let roster = roster_names(manifest);
    let charter = charter_terms(manifest);
    let keys = secret_keys(manifest);
    let labels = classify_labels(item, severity, source);
    let (title, body) = candidate_issue(item, &handle);

    let scrubbed = match scrub(&body, company, secrets, &keys, &roster, &charter).await? {
        ScrubOutcome::Aborted { reason } => return Ok(FeedbackResponse::blocked(&item.id, reason)),
        ScrubOutcome::Ready(body) => body,
    };

    if preview {
        return Ok(FeedbackResponse {
            item_id: item.id.clone(),
            filed: false,
            blocked: false,
            reason: None,
            prefilled_url: Some(manual_issue_url(&filer.repo, &title, &scrubbed, &labels)),
            preview_body: Some(scrubbed),
            issue_url: None,
            deduped: false,
        });
    }

    // A throttled handle (too many low-quality filings) has its standing Auto
    // consent downgraded to Assisted before anything leaves the machine.
    let consent = filer.quality.effective_consent(&handle, filer.consent);

    let outcome = file_feedback(
        filer.client.as_deref(),
        &filer.repo,
        company.as_ref(),
        &title,
        &scrubbed,
        &labels,
        consent,
        &filer.limiter,
    )
    .await?;

    Ok(match outcome {
        FilingOutcome::Filed { url } => {
            // A clean filing counts toward the handle's quality history.
            filer.quality.record_filed(&handle);
            store.update_status(&item.id, &url, "open").await?;
            FeedbackResponse {
                item_id: item.id.clone(),
                filed: true,
                blocked: false,
                reason: None,
                preview_body: None,
                prefilled_url: None,
                issue_url: Some(url),
                deduped: false,
            }
        }
        FilingOutcome::Deduped { url } => {
            // A filing that immediately duplicates an existing issue is a
            // low-quality signal against the filing handle.
            filer.quality.record_filed(&handle);
            filer.quality.record_low_quality(&handle);
            store.update_status(&item.id, &url, "duplicate").await?;
            FeedbackResponse {
                item_id: item.id.clone(),
                filed: true,
                blocked: false,
                reason: None,
                preview_body: None,
                prefilled_url: None,
                issue_url: Some(url),
                deduped: true,
            }
        }
        FilingOutcome::RateLimited => FeedbackResponse {
            item_id: item.id.clone(),
            filed: false,
            blocked: false,
            reason: Some("rate limit reached; try later or file manually".to_string()),
            preview_body: None,
            prefilled_url: None,
            issue_url: None,
            deduped: false,
        },
        FilingOutcome::ManualLink { url } => FeedbackResponse {
            item_id: item.id.clone(),
            filed: false,
            blocked: false,
            reason: None,
            preview_body: None,
            prefilled_url: Some(url),
            issue_url: None,
            deduped: false,
        },
    })
}

/// The roster names/handles to redact (agent ids plus the company `@handle`
/// stem is intentionally *not* redacted — it is the public provenance signer).
fn roster_names(manifest: Option<&CompanyManifest>) -> Vec<String> {
    manifest
        .map(|m| m.agents.iter().map(|a| a.id.clone()).collect())
        .unwrap_or_default()
}

/// Charter specifics mapped to structural descriptions.
fn charter_terms(manifest: Option<&CompanyManifest>) -> Vec<CharterTerm> {
    let mut terms = Vec::new();
    let Some(manifest) = manifest else {
        return terms;
    };
    for skill in &manifest.place.skills {
        if !skill.price_usd.trim().is_empty() {
            terms.push(CharterTerm::new(skill.price_usd.clone(), "a priced skill"));
        }
    }
    if let Some(output) = &manifest.company.output
        && !output.trim().is_empty()
    {
        terms.push(CharterTerm::new(
            output.clone(),
            "the company's stated output",
        ));
    }
    terms
}

/// The `SecretStore` keys whose values must never appear in a filed body.
fn secret_keys(manifest: Option<&CompanyManifest>) -> Vec<String> {
    let mut keys = vec!["github_token".to_string(), "tinyhumans_token".to_string()];
    if let Some(manifest) = manifest {
        for name in manifest.channels.keys() {
            keys.push(format!("channel.{name}.hmac"));
        }
    }
    keys
}

/// Builds the candidate issue `(title, body)` for an item, signing the body
/// with the company `@handle` so preview and post are byte-identical.
fn candidate_issue(item: &FeedbackItem, handle: &str) -> (String, String) {
    let title = format!(
        "[{}] {}",
        item.category.as_str(),
        first_line(&item.operator_words, 72)
    );
    let mut body = String::new();
    body.push_str(&format!("**Category:** {}\n", item.category.as_str()));
    body.push_str(&format!("**Runtime:** v{}\n", item.runtime_version));
    if let (Some(name), version) = (&item.template_name, &item.template_version) {
        let version = version.as_deref().unwrap_or("?");
        body.push_str(&format!("**Template:** {name} {version}\n"));
    }
    if let Some(work) = &item.work_item {
        body.push_str(&format!("**Work item:** {work}\n"));
    }
    body.push('\n');
    body.push_str(&item.context_excerpt);
    body.push_str(&format!("\n\n— filed by @{handle}"));
    (title, body)
}

/// The first line of `s`, truncated to `max` chars.
fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > max {
        line.chars().take(max).collect()
    } else {
        line.to_string()
    }
}
