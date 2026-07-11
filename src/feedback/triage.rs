//! Feedback triage: the full label taxonomy, dedupe/cluster, and consent
//! throttling (`docs/spec/feedback-loop/triage.md`).
//!
//! Where [`labels`](super::labels) mints the label set for a single filed item,
//! this module owns the *taxonomy* itself and the downstream triage behavior a
//! roster-job triage agent performs over the tracker:
//!
//! * [`classify_labels`] — the single source of truth for the four-axis label
//!   set (`type/`, `area/`, `sev/`, `source/`) plus the base `feedback` label.
//! * [`TriageAgent`] — searches existing issues, merges duplicates
//!   ([`TriageAgent::dedupe`]) by commenting on the canonical and closing the
//!   duplicate, and maintains cluster issues
//!   ([`TriageAgent::maintain_cluster`]).
//! * [`cluster_plans`] / [`ClusterPlan::promote`] — group similar issues and
//!   decide when a cluster crosses the `count × severity` promotion threshold.
//! * [`QualityLedger`] — throttles a company's auto-consent after repeated
//!   low-quality agent-filings.
//! * [`escalation_for`] and the release-notes helpers ([`map_fixed_issues`],
//!   [`process_bug_check`]) surface the normative escalation and
//!   "You said, we did" contracts as pure functions the caller acts on.
//!
//! Everything here is exercised offline against
//! [`MockGitHubClient`](super::github::MockGitHubClient); nothing touches the
//! network.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use crate::Result;
use crate::feedback::github::{ExistingIssue, GitHubClient, IssueDraft};
use crate::feedback::labels::area_for;
use crate::feedback::types::{ConsentMode, FeedbackItem};

/// The operator-impact axis of the label taxonomy (`sev/`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// A papercut: annoying but not blocking.
    Annoyance,
    /// The operator is blocked from getting work done.
    Blocked,
    /// The problem cost real money.
    MoneyLost,
}

impl Severity {
    /// The kebab-case wire token used in the `sev/` label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Annoyance => "annoyance",
            Self::Blocked => "blocked",
            Self::MoneyLost => "money-lost",
        }
    }

    /// The promotion weight of this severity (`count × weight` scores a
    /// cluster). Higher severity pulls a cluster over the threshold sooner.
    pub fn weight(self) -> u32 {
        match self {
            Self::Annoyance => 1,
            Self::Blocked => 3,
            Self::MoneyLost => 10,
        }
    }
}

/// The who-filed-it axis of the label taxonomy (`source/`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackSource {
    /// The operator filed it themselves.
    Operator,
    /// The company's brain filed it on the operator's behalf.
    AgentFiled,
    /// The platform filed it (fleet-wide signal).
    Platform,
}

impl FeedbackSource {
    /// The kebab-case wire token used in the `source/` label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::AgentFiled => "agent-filed",
            Self::Platform => "platform",
        }
    }
}

/// Builds the full four-axis label set for a feedback item.
///
/// Every issue carries `feedback` plus exactly one label from each axis:
/// `type/<category>`, `area/<surface>`, `sev/<severity>`, and
/// `source/<who>`. This is the single source of truth the filer and the triage
/// agent both consult.
pub fn classify_labels(
    item: &FeedbackItem,
    severity: Severity,
    source: FeedbackSource,
) -> Vec<String> {
    vec![
        "feedback".to_string(),
        format!("type/{}", item.category.as_str()),
        format!("area/{}", area_for(item)),
        format!("sev/{}", severity.as_str()),
        format!("source/{}", source.as_str()),
    ]
}

/// What a [`TriageAgent::dedupe`] pass decided for one candidate issue.
#[derive(Clone, Debug, PartialEq)]
pub enum DedupePlan {
    /// No earlier match; the candidate stands on its own.
    Distinct,
    /// The candidate duplicates an earlier `canonical` issue; a merge comment
    /// was posted on the canonical and the candidate (`closed`) was closed.
    Merged {
        /// The earlier issue kept as canonical.
        canonical: ExistingIssue,
        /// The candidate issue number that was closed as a duplicate.
        closed: u64,
    },
}

/// A group of similar issues that a triage agent can fold into one cluster.
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterPlan {
    /// The normalized title key the members share.
    pub key: String,
    /// A human-readable summary (the first member's cleaned title).
    pub summary: String,
    /// The member issue numbers, ascending.
    pub members: Vec<u64>,
    /// The number of members.
    pub count: usize,
    /// The cluster issue title, e.g. `"12 reports: email drafts too formal"`.
    pub title: String,
}

impl ClusterPlan {
    /// The promotion score for this cluster at `severity`: `count × weight`.
    pub fn score(&self, severity: Severity) -> u32 {
        (self.count as u32).saturating_mul(severity.weight())
    }

    /// Whether this cluster crosses the promotion `threshold` at `severity`.
    pub fn promote(&self, severity: Severity, threshold: u32) -> bool {
        self.score(severity) >= threshold
    }
}

/// The escalation flags a filed issue's labels imply (`triage.md`).
///
/// Pure: the caller executes the side effects (paging, mirroring). We only
/// surface the decision so it stays inspectable and testable offline.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EscalationPlan {
    /// `sev/money-lost` issues page maintainers.
    pub pages_maintainers: bool,
    /// `area/brain` issues that reproduce upstream mirror to the owning repo.
    pub mirror_to: Option<String>,
}

/// Computes the [`EscalationPlan`] implied by an issue's `labels`.
pub fn escalation_for(labels: &[String]) -> EscalationPlan {
    let pages_maintainers = labels.iter().any(|l| l == "sev/money-lost");
    let mirror_to = labels
        .iter()
        .any(|l| l == "area/brain")
        .then(|| "tinyhumansai/medulla".to_string());
    EscalationPlan {
        pages_maintainers,
        mirror_to,
    }
}

/// Groups similar existing issues into candidate clusters.
///
/// Issues are keyed by a normalized title (lowercased, any leading `[type]`
/// prefix stripped, whitespace collapsed); a key with two or more members
/// becomes a [`ClusterPlan`]. Single issues are not clusters and are omitted.
/// Returned deterministically ordered by key.
pub fn cluster_plans(issues: &[ExistingIssue]) -> Vec<ClusterPlan> {
    let mut groups: HashMap<String, (String, Vec<u64>)> = HashMap::new();
    for issue in issues {
        let key = normalize_title(&issue.title);
        let entry = groups
            .entry(key)
            .or_insert_with(|| (strip_type_prefix(&issue.title), Vec::new()));
        entry.1.push(issue.number);
    }

    let mut plans: Vec<ClusterPlan> = groups
        .into_iter()
        .filter(|(_, (_, members))| members.len() >= 2)
        .map(|(key, (summary, mut members))| {
            members.sort_unstable();
            let count = members.len();
            let title = format!("{count} reports: {summary}");
            ClusterPlan {
                key,
                summary,
                members,
                count,
                title,
            }
        })
        .collect();
    plans.sort_by(|a, b| a.key.cmp(&b.key));
    plans
}

/// Normalizes a title into a clustering key.
fn normalize_title(title: &str) -> String {
    strip_type_prefix(title)
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strips a leading `[type] ` prefix (as minted by the filer) from a title.
fn strip_type_prefix(title: &str) -> String {
    let trimmed = title.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(idx) = rest.find(']')
    {
        return rest[idx + 1..].trim().to_string();
    }
    trimmed.to_string()
}

/// A roster-job triage agent operating over one repo's issue tracker.
///
/// Not kernel plumbing: it is a helper a triage company (or a maintenance
/// sweep) drives against a [`GitHubClient`]. Every method is offline-testable
/// against the mock client.
pub struct TriageAgent {
    client: Arc<dyn GitHubClient>,
    repo: String,
}

impl TriageAgent {
    /// Builds a triage agent filing against `repo` through `client`.
    pub fn new(client: Arc<dyn GitHubClient>, repo: impl Into<String>) -> Self {
        Self {
            client,
            repo: repo.into(),
        }
    }

    /// Merges `candidate` into an earlier canonical issue if one exists.
    ///
    /// Searches the tracker for the candidate's title; if an *earlier* issue
    /// matches, comments on that canonical noting the merge and closes the
    /// candidate, returning [`DedupePlan::Merged`]. Otherwise the candidate is
    /// [`DedupePlan::Distinct`].
    pub async fn dedupe(&self, candidate: &ExistingIssue) -> Result<DedupePlan> {
        let hits = self
            .client
            .search_issues(&self.repo, &candidate.title)
            .await?;
        // The canonical is the earliest matching issue other than the candidate.
        let canonical = hits
            .into_iter()
            .filter(|issue| issue.number != candidate.number)
            .min_by_key(|issue| issue.number);

        match canonical {
            Some(canonical) => {
                self.client
                    .comment_issue(
                        &self.repo,
                        canonical.number,
                        &format!(
                            "Folding in duplicate #{} — same report as this issue.",
                            candidate.number
                        ),
                    )
                    .await?;
                self.client
                    .close_issue(&self.repo, candidate.number)
                    .await?;
                Ok(DedupePlan::Merged {
                    canonical,
                    closed: candidate.number,
                })
            }
            None => Ok(DedupePlan::Distinct),
        }
    }

    /// Materializes a [`ClusterPlan`]: opens a cluster issue, then comments on
    /// and closes every member, pointing each at the cluster.
    ///
    /// `area_label` is the owning `area/<…>` value (e.g.
    /// `template:marketing_agency`); it is appended to the cluster title and
    /// applied as an `area/` label. Returns the created cluster issue URL.
    pub async fn maintain_cluster(&self, plan: &ClusterPlan, area_label: &str) -> Result<String> {
        let title = format!("{} — {}", plan.title, area_label);
        let body = format!(
            "Cluster of {} similar reports: {}.\n\nMembers: {}",
            plan.count,
            plan.summary,
            plan.members
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let cluster_url = self
            .client
            .create_issue(
                &self.repo,
                &IssueDraft {
                    title,
                    body,
                    labels: vec!["feedback".to_string(), format!("area/{area_label}")],
                },
            )
            .await?;

        for member in &plan.members {
            self.client
                .comment_issue(
                    &self.repo,
                    *member,
                    &format!("Folded into cluster: {cluster_url}"),
                )
                .await?;
            self.client.close_issue(&self.repo, *member).await?;
        }
        Ok(cluster_url)
    }
}

impl std::fmt::Debug for TriageAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TriageAgent")
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

/// Per-handle filing quality, used to throttle auto-consent.
///
/// A company that repeatedly files low-quality issues (e.g. filings that are
/// immediately closed as duplicates, or maintainer-flagged) has its `Auto`
/// consent downgraded to `Assisted` once its low-quality ratio crosses a
/// threshold, so the operator confirms each filing again. In-memory, mirroring
/// [`RateLimiter`](super::github::RateLimiter).
#[derive(Debug)]
pub struct QualityLedger {
    threshold: f64,
    min_samples: usize,
    filings: StdMutex<HashMap<String, Counts>>,
}

/// The per-handle filing tally.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    filed: usize,
    low_quality: usize,
}

impl QualityLedger {
    /// Builds a ledger that downgrades `Auto` consent once a handle has at
    /// least `min_samples` filings and a low-quality ratio at or above
    /// `threshold` (0.0–1.0).
    pub fn new(threshold: f64, min_samples: usize) -> Self {
        Self {
            threshold,
            min_samples,
            filings: StdMutex::new(HashMap::new()),
        }
    }

    /// Records that `handle` filed an issue (of any quality).
    pub fn record_filed(&self, handle: &str) {
        let mut filings = self.filings.lock().expect("quality ledger poisoned");
        filings.entry(handle.to_string()).or_default().filed += 1;
    }

    /// Records that `handle`'s most recent filing was low-quality (e.g. an
    /// immediate duplicate). Callers still call [`record_filed`](Self::record_filed)
    /// for the same filing; this only bumps the low-quality tally.
    pub fn record_low_quality(&self, handle: &str) {
        let mut filings = self.filings.lock().expect("quality ledger poisoned");
        filings.entry(handle.to_string()).or_default().low_quality += 1;
    }

    /// The consent mode `handle` effectively gets given its filing history.
    ///
    /// Only `Auto` is subject to throttling; `Manual`/`Assisted` pass through
    /// unchanged. An `Auto` handle over the low-quality threshold is downgraded
    /// to `Assisted`.
    pub fn effective_consent(&self, handle: &str, configured: ConsentMode) -> ConsentMode {
        if configured != ConsentMode::Auto {
            return configured;
        }
        let filings = self.filings.lock().expect("quality ledger poisoned");
        if let Some(counts) = filings.get(handle)
            && counts.filed >= self.min_samples
            && (counts.low_quality as f64 / counts.filed as f64) >= self.threshold
        {
            return ConsentMode::Assisted;
        }
        configured
    }
}

impl Default for QualityLedger {
    /// Downgrades after 3+ filings with at least half low-quality.
    fn default() -> Self {
        Self::new(0.5, 3)
    }
}

/// Joins fixed issue URLs to the feedback items that caused them.
///
/// The "You said, we did" contract: given the issue URLs closed by a release
/// and the company's feedback items, returns each `(issue_url, item)` pair
/// whose item links that issue, so the caller can surface *"things you flagged
/// were fixed"* to the right operators.
pub fn map_fixed_issues<'a>(
    fixed: &[String],
    items: &'a [FeedbackItem],
) -> Vec<(String, &'a FeedbackItem)> {
    let mut out = Vec::new();
    for url in fixed {
        for item in items {
            if item.filed_issue_url.as_deref() == Some(url.as_str()) {
                out.push((url.clone(), item));
            }
        }
    }
    out
}

/// Flags release-notes process bugs.
///
/// A `feedback`-labeled issue closed by a release but absent from the release
/// notes is a process bug (`triage.md`). Returns every closed feedback issue
/// URL missing from `notes_issue_urls`.
pub fn process_bug_check(
    closed_feedback_issues: &[String],
    notes_issue_urls: &[String],
) -> Vec<String> {
    closed_feedback_issues
        .iter()
        .filter(|url| !notes_issue_urls.iter().any(|n| n == *url))
        .cloned()
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::feedback::github::MockGitHubClient;
    use crate::feedback::types::{FeedbackCategory, FeedbackInput};

    fn item(category: FeedbackCategory, template: Option<&str>) -> FeedbackItem {
        FeedbackItem::capture(
            FeedbackInput {
                category,
                note: "n".into(),
                work_ref: None,
                template_name: template.map(str::to_string),
                template_version: None,
            },
            "0.1.0",
            ConsentMode::Auto,
        )
    }

    #[test]
    fn classify_covers_all_four_axes() {
        let labels = classify_labels(
            &item(FeedbackCategory::Bug, None),
            Severity::Blocked,
            FeedbackSource::Operator,
        );
        assert!(labels.contains(&"feedback".to_string()));
        assert!(labels.contains(&"type/bug".to_string()));
        assert!(labels.contains(&"area/runtime".to_string()));
        assert!(labels.contains(&"sev/blocked".to_string()));
        assert!(labels.contains(&"source/operator".to_string()));
        // Exactly one label per axis plus the base label.
        assert_eq!(labels.len(), 5);
    }

    #[test]
    fn classify_names_template_area_and_money_lost() {
        let labels = classify_labels(
            &item(FeedbackCategory::TemplateGap, Some("marketing_agency")),
            Severity::MoneyLost,
            FeedbackSource::AgentFiled,
        );
        assert!(labels.contains(&"area/template:marketing_agency".to_string()));
        assert!(labels.contains(&"sev/money-lost".to_string()));
        assert!(labels.contains(&"source/agent-filed".to_string()));
        assert!(labels.contains(&"type/template-gap".to_string()));
    }

    #[test]
    fn severity_and_source_wire_tokens_are_exact() {
        assert_eq!(Severity::Annoyance.as_str(), "annoyance");
        assert_eq!(Severity::Blocked.as_str(), "blocked");
        assert_eq!(Severity::MoneyLost.as_str(), "money-lost");
        assert_eq!(FeedbackSource::Operator.as_str(), "operator");
        assert_eq!(FeedbackSource::AgentFiled.as_str(), "agent-filed");
        assert_eq!(FeedbackSource::Platform.as_str(), "platform");
    }

    #[tokio::test]
    async fn dedupe_comments_on_canonical_and_closes_duplicate() {
        let client = Arc::new(
            MockGitHubClient::new()
                .with_existing(42, "https://gh/issues/42", "email drafts too formal")
                .with_existing(57, "https://gh/issues/57", "email drafts too formal"),
        );
        let agent = TriageAgent::new(client.clone(), "acme/repo");
        let candidate = ExistingIssue {
            number: 57,
            url: "https://gh/issues/57".into(),
            title: "email drafts too formal".into(),
        };

        let plan = agent.dedupe(&candidate).await.unwrap();
        match plan {
            DedupePlan::Merged { canonical, closed } => {
                assert_eq!(canonical.number, 42);
                assert_eq!(closed, 57);
            }
            other => panic!("expected Merged, got {other:?}"),
        }
        // Commented on the canonical, closed the duplicate, created nothing.
        assert_eq!(client.comments().len(), 1);
        assert_eq!(client.comments()[0].0, 42);
        assert_eq!(client.closed(), vec![57]);
        assert!(client.created().is_empty());
    }

    #[tokio::test]
    async fn dedupe_leaves_a_distinct_issue_untouched() {
        let client = Arc::new(MockGitHubClient::new().with_existing(
            9,
            "https://gh/issues/9",
            "unique problem",
        ));
        let agent = TriageAgent::new(client.clone(), "acme/repo");
        let candidate = ExistingIssue {
            number: 9,
            url: "https://gh/issues/9".into(),
            title: "unique problem".into(),
        };
        assert_eq!(
            agent.dedupe(&candidate).await.unwrap(),
            DedupePlan::Distinct
        );
        assert!(client.comments().is_empty());
        assert!(client.closed().is_empty());
    }

    #[test]
    fn cluster_plans_group_similar_titles_and_score() {
        let issues = vec![
            ExistingIssue {
                number: 3,
                url: "u3".into(),
                title: "[wrong-output] email drafts too formal".into(),
            },
            ExistingIssue {
                number: 1,
                url: "u1".into(),
                title: "Email drafts too formal".into(),
            },
            ExistingIssue {
                number: 2,
                url: "u2".into(),
                title: "email  drafts   too formal".into(),
            },
            ExistingIssue {
                number: 8,
                url: "u8".into(),
                title: "a lone report".into(),
            },
        ];
        let plans = cluster_plans(&issues);
        assert_eq!(plans.len(), 1, "only the 3-member group clusters");
        let plan = &plans[0];
        assert_eq!(plan.count, 3);
        assert_eq!(plan.members, vec![1, 2, 3]);
        assert!(plan.title.starts_with("3 reports:"));
        // count × severity.
        assert_eq!(plan.score(Severity::Annoyance), 3);
        assert_eq!(plan.score(Severity::MoneyLost), 30);
        assert!(!plan.promote(Severity::Annoyance, 10));
        assert!(plan.promote(Severity::MoneyLost, 10));
    }

    #[tokio::test]
    async fn maintain_cluster_creates_issue_and_closes_members() {
        let client = Arc::new(MockGitHubClient::new());
        let agent = TriageAgent::new(client.clone(), "acme/repo");
        let plan = ClusterPlan {
            key: "email drafts too formal".into(),
            summary: "email drafts too formal".into(),
            members: vec![1, 2, 3],
            count: 3,
            title: "3 reports: email drafts too formal".into(),
        };
        let url = agent
            .maintain_cluster(&plan, "template:marketing_agency")
            .await
            .unwrap();
        assert!(url.starts_with("https://github.com/mock/issues/"));
        let created = client.created();
        assert_eq!(created.len(), 1);
        assert!(created[0].title.contains("template:marketing_agency"));
        assert!(
            created[0]
                .labels
                .contains(&"area/template:marketing_agency".to_string())
        );
        // Every member was commented on and closed.
        assert_eq!(client.closed(), vec![1, 2, 3]);
        assert_eq!(client.comments().len(), 3);
    }

    #[test]
    fn throttle_downgrades_auto_after_low_quality_filings() {
        let ledger = QualityLedger::new(0.5, 2);
        // Below the sample floor: no downgrade yet.
        ledger.record_filed("noisy");
        ledger.record_low_quality("noisy");
        assert_eq!(
            ledger.effective_consent("noisy", ConsentMode::Auto),
            ConsentMode::Auto
        );
        // Second low-quality filing crosses the floor and the ratio.
        ledger.record_filed("noisy");
        ledger.record_low_quality("noisy");
        assert_eq!(
            ledger.effective_consent("noisy", ConsentMode::Auto),
            ConsentMode::Assisted
        );
        // A clean handle keeps Auto; non-Auto modes always pass through.
        ledger.record_filed("clean");
        ledger.record_filed("clean");
        assert_eq!(
            ledger.effective_consent("clean", ConsentMode::Auto),
            ConsentMode::Auto
        );
        assert_eq!(
            ledger.effective_consent("noisy", ConsentMode::Manual),
            ConsentMode::Manual
        );
    }

    #[test]
    fn escalation_flags_money_lost_and_brain() {
        let money = escalation_for(&["sev/money-lost".to_string(), "area/product".to_string()]);
        assert!(money.pages_maintainers);
        assert!(money.mirror_to.is_none());

        let brain = escalation_for(&["area/brain".to_string(), "sev/annoyance".to_string()]);
        assert!(!brain.pages_maintainers);
        assert_eq!(brain.mirror_to.as_deref(), Some("tinyhumansai/medulla"));
    }

    #[test]
    fn release_notes_map_and_process_bug_check() {
        let mut filed = item(FeedbackCategory::Bug, None);
        filed.filed_issue_url = Some("https://gh/issues/7".into());
        let other = item(FeedbackCategory::Docs, None);
        let items = vec![filed, other];

        let mapped = map_fixed_issues(&["https://gh/issues/7".to_string()], &items);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].0, "https://gh/issues/7");
        assert_eq!(mapped[0].1.category, FeedbackCategory::Bug);

        // A closed feedback issue missing from the notes is a process bug.
        let bugs = process_bug_check(
            &[
                "https://gh/issues/7".to_string(),
                "https://gh/issues/9".to_string(),
            ],
            &["https://gh/issues/7".to_string()],
        );
        assert_eq!(bugs, vec!["https://gh/issues/9".to_string()]);
    }
}
