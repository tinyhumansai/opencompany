//! The [`ArtifactStore`] port: versioned task outputs and the human-edit diff.
//!
//! A task produces things — a draft, a post, a generated file. Before this port
//! those outputs were folded into the card's `note` as plain text
//! (`append_result`), which loses two things worth keeping:
//!
//! * **Version history.** A task that runs, gets redirected, and re-runs just
//!   append-stacks more text into the note. There is no "the second draft
//!   replaced the first" — only a growing blob.
//! * **What a human changed before approving.** "The agent wrote X, the
//!   operator shipped Y" is the highest-signal quality datum the product can
//!   produce: heavy editing means the agent's instructions need work. Folded
//!   into note text, it is unrecoverable.
//!
//! So an artifact is a record with an ordered [`ArtifactVersion`] list, and an
//! operator edit is *a new version by a different author* rather than a
//! mutation of the old one. Nothing is ever overwritten, which is what makes
//! [`ArtifactRecord::human_edit_diff`] answerable at any later point.
//!
//! Independent of the per-task timeline (#185): a version may record which
//! timeline step produced it via [`ArtifactVersion::step_seq`], but that field
//! is optional and nothing here reads the event journal.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// What an artifact holds. Drives the console's renderer choice; the body is
/// always text on the wire (a binary artifact carries a reference, not bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    /// Plain text — the default for an agent reply.
    Text,
    /// Markdown source (a draft, a post).
    Markdown,
    /// A reference to a generated image (URL or workspace path, not bytes).
    Image,
    /// A reference to a generated file (workspace path, not bytes).
    File,
}

impl ArtifactKind {
    /// The stable wire word, for logs and route bodies.
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Text => "text",
            ArtifactKind::Markdown => "markdown",
            ArtifactKind::Image => "image",
            ArtifactKind::File => "file",
        }
    }
}

/// Who produced a version.
///
/// The whole point of the port: this is what makes an operator's pre-approval
/// edit distinguishable from the agent's own re-run, and therefore what makes
/// the quality signal computable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactAuthor {
    /// An agent turn produced it.
    Agent,
    /// A human edited it (typically just before approving).
    Operator,
}

/// One immutable revision of an artifact.
///
/// Versions are append-only and 1-based. A version is never edited in place —
/// an operator's change appends a new one — so the full provenance chain
/// survives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactVersion {
    /// 1-based revision number, contiguous within the artifact.
    pub version: u32,
    /// The revision's content.
    pub body: String,
    /// Whether an agent or a human wrote this revision.
    pub author: ArtifactAuthor,
    /// The agent id, or the operator's label. Free-form and display-only.
    pub author_id: String,
    /// Epoch-millis the revision was recorded.
    pub created_at_millis: u64,
    /// The journal sequence of the timeline step that produced it, when known
    /// (#185's `task_id`-correlated events). Optional and purely a
    /// cross-reference — this port never reads the event log, so an artifact
    /// stands on its own without the timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_seq: Option<u64>,
    /// Why this revision exists (e.g. `"operator edit before approval"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The task **attempt** ([`RunRecord`](crate::ports::runs::RunRecord)) that
    /// produced this revision, when an agent turn under one did (issue #242).
    ///
    /// Per *version*, not per record, because that is the granularity the
    /// question has: a card dispatched twice produces v1 under the first attempt
    /// and v2 under the second, and a record-level field would remember only the
    /// later one — losing the link the first attempt's row needs to point at
    /// what it actually wrote. The record-level answer is still available as
    /// `latest().run_id`.
    ///
    /// `None` for an operator edit (no attempt produced it), for a revision
    /// written by an untracked dispatch, and for every artifact stored before
    /// this field existed. Artifacts are persisted as a JSON blob on every
    /// backend (`artifact_json` on sqlite, a document on MongoDB, a file on the
    /// filesystem), so this needs **no schema migration** and a pre-#242 record
    /// loads with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// A versioned output of one task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRecord {
    /// Stable id within the company.
    pub id: String,
    /// The task that produced it.
    pub task_id: String,
    /// A short display title.
    pub title: String,
    /// What it holds.
    pub kind: ArtifactKind,
    /// Every revision, oldest first. Never empty in practice — an artifact is
    /// created with its first version — but the accessors tolerate empty so a
    /// hand-written or truncated record cannot panic a read route.
    pub versions: Vec<ArtifactVersion>,
    /// Epoch-millis the artifact was first created.
    pub created_at_millis: u64,
    /// Epoch-millis of the newest revision.
    pub updated_at_millis: u64,
}

impl ArtifactRecord {
    /// Creates an artifact with its first (agent-authored) version.
    pub fn new(
        id: impl Into<String>,
        task_id: impl Into<String>,
        title: impl Into<String>,
        kind: ArtifactKind,
        body: impl Into<String>,
        author_id: impl Into<String>,
        at_millis: u64,
    ) -> Self {
        let id = id.into();
        Self {
            id,
            task_id: task_id.into(),
            title: title.into(),
            kind,
            versions: vec![ArtifactVersion {
                version: 1,
                body: body.into(),
                author: ArtifactAuthor::Agent,
                author_id: author_id.into(),
                created_at_millis: at_millis,
                step_seq: None,
                note: None,
                run_id: None,
            }],
            created_at_millis: at_millis,
            updated_at_millis: at_millis,
        }
    }

    /// The newest revision, or `None` on an empty record.
    pub fn latest(&self) -> Option<&ArtifactVersion> {
        self.versions.last()
    }

    /// A revision by its 1-based number.
    pub fn version(&self, version: u32) -> Option<&ArtifactVersion> {
        self.versions.iter().find(|v| v.version == version)
    }

    /// Appends a revision, numbering it after the current newest and advancing
    /// `updated_at_millis`. Returns the new version number.
    ///
    /// Numbering is derived from the existing max rather than `len()`, so a
    /// record that ever lost a row mid-history still numbers monotonically
    /// instead of silently reusing a number that already means something else.
    pub fn push_version(
        &mut self,
        body: impl Into<String>,
        author: ArtifactAuthor,
        author_id: impl Into<String>,
        at_millis: u64,
        note: Option<String>,
    ) -> u32 {
        let next = self.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1;
        self.versions.push(ArtifactVersion {
            version: next,
            body: body.into(),
            author,
            author_id: author_id.into(),
            created_at_millis: at_millis,
            step_seq: None,
            note,
            run_id: None,
        });
        self.updated_at_millis = at_millis;
        next
    }

    /// Stamps the newest revision with the attempt that produced it (#242).
    ///
    /// A separate call rather than a sixth parameter on
    /// [`push_version`](Self::push_version) / [`new`](Self::new): only the
    /// dispatch path has a run to name, and widening those signatures would make
    /// every operator-edit and test call site pass a `None` it has no meaning
    /// for. A no-op on an empty record.
    pub fn stamp_run(&mut self, run_id: impl Into<String>) {
        if let Some(latest) = self.versions.last_mut() {
            latest.run_id = Some(run_id.into());
        }
    }

    /// The diff of what a human changed before approving: the last
    /// agent-authored revision → the last operator-authored revision after it.
    ///
    /// `None` when no human has edited (the common, healthy case) or when no
    /// agent version precedes the edit. Anchoring on the last agent version
    /// *before* the operator's — rather than simply v1 → latest — is what keeps
    /// the diff meaningful across a redirect: if the agent re-ran and then a
    /// human tweaked the second draft, the interesting change is against that
    /// second draft, not the abandoned first.
    pub fn human_edit_diff(&self) -> Option<ArtifactDiff> {
        let last_operator = self
            .versions
            .iter()
            .rev()
            .find(|v| v.author == ArtifactAuthor::Operator)?;
        let last_agent_before = self
            .versions
            .iter()
            .rfind(|v| v.author == ArtifactAuthor::Agent && v.version < last_operator.version)?;
        Some(ArtifactDiff::between(last_agent_before, last_operator))
    }
}

/// One line of a diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffOp {
    /// Unchanged, present in both sides.
    Equal,
    /// Only in the newer side.
    Insert,
    /// Only in the older side.
    Delete,
}

/// A single diff line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    /// What happened to this line.
    pub op: DiffOp,
    /// The line's text (no trailing newline).
    pub text: String,
}

/// A line-level diff between two revisions, plus the churn summary that makes
/// it a quality signal rather than just a rendering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactDiff {
    /// The older revision's number.
    pub from_version: u32,
    /// The newer revision's number.
    pub to_version: u32,
    /// The line-by-line diff, in newer-side order with deletions interleaved.
    pub lines: Vec<DiffLine>,
    /// Lines only in the newer side.
    pub added: usize,
    /// Lines only in the older side.
    pub removed: usize,
    /// Fraction of the older side's lines that changed, 0.0–1.0.
    ///
    /// This is the number worth alerting on: sustained high churn on an agent's
    /// artifacts means its instructions need work. Defined against the older
    /// (agent) side so "the operator rewrote most of it" reads as ~1.0. An
    /// empty original with added lines is 1.0 rather than a divide-by-zero.
    pub churn: f64,
}

impl ArtifactDiff {
    /// Diffs two revisions by line.
    pub fn between(from: &ArtifactVersion, to: &ArtifactVersion) -> Self {
        let lines = diff_lines(&from.body, &to.body);
        let added = lines.iter().filter(|l| l.op == DiffOp::Insert).count();
        let removed = lines.iter().filter(|l| l.op == DiffOp::Delete).count();
        let original = from.body.lines().count();
        let churn = if original == 0 {
            if added == 0 { 0.0 } else { 1.0 }
        } else {
            (removed as f64 / original as f64).min(1.0)
        };
        Self {
            from_version: from.version,
            to_version: to.version,
            lines,
            added,
            removed,
            churn,
        }
    }
}

/// Line-level diff via a longest-common-subsequence table.
///
/// Hand-rolled on purpose: the crate takes no diff dependency, and a line LCS
/// is a dozen lines of DP. Quadratic in line count, which is fine for artifact
/// bodies (drafts and posts, not repositories) — and bounded below by
/// [`MAX_DIFF_LINES`], past which the diff degrades to a whole-body
/// replace rather than allocating an enormous table.
fn diff_lines(from: &str, to: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = from.lines().collect();
    let b: Vec<&str> = to.lines().collect();

    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        // Degrade rather than build an n×m table over huge inputs. Still a
        // truthful diff — every old line removed, every new line added.
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend(a.iter().map(|l| DiffLine {
            op: DiffOp::Delete,
            text: (*l).to_string(),
        }));
        out.extend(b.iter().map(|l| DiffLine {
            op: DiffOp::Insert,
            text: (*l).to_string(),
        }));
        return out;
    }

    // lcs[i][j] = length of the LCS of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(DiffLine {
                op: DiffOp::Equal,
                text: a[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine {
                op: DiffOp::Delete,
                text: a[i].to_string(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                op: DiffOp::Insert,
                text: b[j].to_string(),
            });
            j += 1;
        }
    }
    for line in a.iter().skip(i) {
        out.push(DiffLine {
            op: DiffOp::Delete,
            text: (*line).to_string(),
        });
    }
    for line in b.iter().skip(j) {
        out.push(DiffLine {
            op: DiffOp::Insert,
            text: (*line).to_string(),
        });
    }
    out
}

/// Line ceiling past which [`diff_lines`] degrades to a whole-body replace
/// instead of building a quadratic LCS table.
pub const MAX_DIFF_LINES: usize = 2_000;

/// Durable per-company task artifacts. Company A's artifacts MUST be invisible
/// to company B.
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// Lists artifacts, most-recently-updated first, optionally narrowed to one
    /// task.
    async fn list(&self, company: &CompanyId, task_id: Option<&str>)
    -> Result<Vec<ArtifactRecord>>;
    /// Fetches one artifact by id, with its full version history.
    async fn get(&self, company: &CompanyId, id: &str) -> Result<Option<ArtifactRecord>>;
    /// Inserts or replaces an artifact by id.
    async fn upsert(&self, company: &CompanyId, artifact: &ArtifactRecord) -> Result<()>;
    /// Deletes an artifact by id; returns whether one was removed.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
}

#[cfg(test)]
mod test {
    use super::*;

    fn version(n: u32, body: &str, author: ArtifactAuthor) -> ArtifactVersion {
        ArtifactVersion {
            version: n,
            body: body.to_string(),
            author,
            author_id: "ceo".to_string(),
            created_at_millis: n as u64,
            step_seq: None,
            note: None,
            run_id: None,
        }
    }

    /// Issue #242: the attempt that wrote a revision is recorded **on that
    /// revision**, so a card dispatched twice keeps both links rather than the
    /// second attempt erasing the first's. And because artifacts are stored as a
    /// JSON blob on every backend, a record written before the field existed
    /// loads with `None` — no schema migration, no backfill.
    #[test]
    fn each_revision_remembers_the_attempt_that_wrote_it() {
        let mut a = ArtifactRecord::new("a1", "t-1", "Draft", ArtifactKind::Text, "one", "ceo", 1);
        a.stamp_run("run-1");
        a.push_version("two", ArtifactAuthor::Agent, "ceo", 2, None);
        a.stamp_run("run-2");
        // An operator edit names no attempt — nobody's run produced it.
        a.push_version("three", ArtifactAuthor::Operator, "operator", 3, None);

        assert_eq!(a.version(1).unwrap().run_id.as_deref(), Some("run-1"));
        assert_eq!(
            a.version(2).unwrap().run_id.as_deref(),
            Some("run-2"),
            "the second attempt must not overwrite the first attempt's link"
        );
        assert_eq!(a.version(3).unwrap().run_id, None);

        let json = serde_json::to_string(&a).expect("serialize");
        assert_eq!(a, serde_json::from_str(&json).expect("round trip"));
        assert!(json.contains(r#""runId":"run-1""#), "{json}");

        // A pre-#242 blob is exactly what an unstamped record serializes to —
        // the field is skipped when absent — so it must still load, with `None`.
        let unstamped =
            ArtifactRecord::new("a2", "t-1", "Draft", ArtifactKind::Text, "one", "ceo", 1);
        let legacy = serde_json::to_string(&unstamped).expect("serialize");
        assert!(!legacy.contains("runId"), "{legacy}");
        let loaded: ArtifactRecord =
            serde_json::from_str(&legacy).expect("a pre-#242 artifact blob must still load");
        assert!(loaded.versions.iter().all(|v| v.run_id.is_none()));
    }

    /// Stamping an artifact with no revisions is a no-op rather than a panic —
    /// the accessors already tolerate an empty record, and so must this.
    #[test]
    fn stamping_an_empty_record_is_a_no_op() {
        let mut a = ArtifactRecord::new("a1", "t-1", "Draft", ArtifactKind::Text, "one", "ceo", 1);
        a.versions.clear();
        a.stamp_run("run-1");
        assert!(a.versions.is_empty());
    }

    #[test]
    fn push_version_numbers_after_the_current_max() {
        let mut a = ArtifactRecord::new("a1", "t-1", "Draft", ArtifactKind::Text, "one", "ceo", 1);
        assert_eq!(a.latest().unwrap().version, 1);
        let n = a.push_version("two", ArtifactAuthor::Operator, "operator", 2, None);
        assert_eq!(n, 2);
        assert_eq!(a.updated_at_millis, 2);
        assert_eq!(a.version(1).unwrap().body, "one");
        assert_eq!(a.latest().unwrap().body, "two");
    }

    /// Numbering must come from the max, not `len()`: a record that lost a row
    /// would otherwise reuse a number that already means a different revision.
    #[test]
    fn push_version_does_not_reuse_a_number_after_a_gap() {
        let mut a = ArtifactRecord::new("a1", "t-1", "Draft", ArtifactKind::Text, "one", "ceo", 1);
        a.push_version("two", ArtifactAuthor::Agent, "ceo", 2, None);
        a.push_version("three", ArtifactAuthor::Agent, "ceo", 3, None);
        a.versions.remove(1); // v2 lost; len() is now 2
        assert_eq!(
            a.push_version("four", ArtifactAuthor::Agent, "ceo", 4, None),
            4
        );
    }

    #[test]
    fn no_human_edit_means_no_diff() {
        let mut a = ArtifactRecord::new("a1", "t-1", "Draft", ArtifactKind::Text, "one", "ceo", 1);
        a.push_version("one revised", ArtifactAuthor::Agent, "ceo", 2, None);
        assert!(a.human_edit_diff().is_none());
    }

    /// The diff anchors on the last *agent* version before the human's, not on
    /// v1 — otherwise a redirect (agent re-runs, then a human tweaks the second
    /// draft) would diff against the abandoned first draft.
    #[test]
    fn human_edit_diff_anchors_on_the_agent_version_it_edited() {
        let mut a = ArtifactRecord::new(
            "a1",
            "t-1",
            "Draft",
            ArtifactKind::Text,
            "first draft",
            "ceo",
            1,
        );
        a.push_version("second draft", ArtifactAuthor::Agent, "ceo", 2, None);
        a.push_version(
            "second draft, edited",
            ArtifactAuthor::Operator,
            "operator",
            3,
            None,
        );

        let diff = a.human_edit_diff().expect("a human edited");
        assert_eq!(
            diff.from_version, 2,
            "must anchor on the edited draft, not v1"
        );
        assert_eq!(diff.to_version, 3);
    }

    #[test]
    fn diff_reports_added_removed_and_churn() {
        let from = version(1, "alpha\nbeta\ngamma", ArtifactAuthor::Agent);
        let to = version(2, "alpha\nBETA\ngamma", ArtifactAuthor::Operator);
        let diff = ArtifactDiff::between(&from, &to);

        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        // One of three original lines changed.
        assert!(
            (diff.churn - 1.0 / 3.0).abs() < 1e-9,
            "churn {}",
            diff.churn
        );
        // Unchanged lines survive as context.
        assert_eq!(
            diff.lines.iter().filter(|l| l.op == DiffOp::Equal).count(),
            2
        );
    }

    #[test]
    fn an_untouched_body_diffs_to_all_equal() {
        let from = version(1, "same\nlines", ArtifactAuthor::Agent);
        let to = version(2, "same\nlines", ArtifactAuthor::Operator);
        let diff = ArtifactDiff::between(&from, &to);
        assert_eq!(diff.added, 0);
        assert_eq!(diff.removed, 0);
        assert_eq!(diff.churn, 0.0);
        assert!(diff.lines.iter().all(|l| l.op == DiffOp::Equal));
    }

    /// A wholesale rewrite is the signal the epic cares about most, so pin that
    /// it reads as maximum churn rather than something merely large.
    #[test]
    fn a_full_rewrite_is_maximum_churn() {
        let from = version(1, "alpha\nbeta", ArtifactAuthor::Agent);
        let to = version(2, "totally\ndifferent", ArtifactAuthor::Operator);
        let diff = ArtifactDiff::between(&from, &to);
        assert_eq!(diff.churn, 1.0);
    }

    #[test]
    fn an_empty_original_does_not_divide_by_zero() {
        let empty = version(1, "", ArtifactAuthor::Agent);
        let filled = version(2, "now it says something", ArtifactAuthor::Operator);
        assert_eq!(ArtifactDiff::between(&empty, &filled).churn, 1.0);
        assert_eq!(ArtifactDiff::between(&empty, &empty).churn, 0.0);
    }

    /// Past the ceiling the diff degrades to a whole-body replace instead of
    /// allocating an n×m table; it must stay truthful, not silently empty.
    #[test]
    fn an_oversized_body_degrades_to_a_whole_body_replace() {
        let big = (0..MAX_DIFF_LINES + 1)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let from = version(1, &big, ArtifactAuthor::Agent);
        let to = version(2, "one line", ArtifactAuthor::Operator);
        let diff = ArtifactDiff::between(&from, &to);
        assert_eq!(diff.removed, MAX_DIFF_LINES + 1);
        assert_eq!(diff.added, 1);
        assert!(diff.lines.iter().all(|l| l.op != DiffOp::Equal));
    }

    #[test]
    fn a_record_round_trips_through_json() {
        let mut a = ArtifactRecord::new(
            "a1",
            "t-1",
            "Launch post",
            ArtifactKind::Markdown,
            "# Draft",
            "ceo",
            1,
        );
        a.push_version(
            "# Draft, edited",
            ArtifactAuthor::Operator,
            "operator",
            2,
            Some("operator edit before approval".to_string()),
        );
        let json = serde_json::to_string(&a).unwrap();
        // camelCase on the wire, matching every other console-facing record.
        assert!(json.contains("\"taskId\""));
        assert!(json.contains("\"createdAtMillis\""));
        let back: ArtifactRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    /// `step_seq` is the only cross-reference to #185 and is optional, so an
    /// artifact must load and serialize without it.
    #[test]
    fn step_seq_is_optional_and_omitted_when_absent() {
        let a = ArtifactRecord::new("a1", "t-1", "D", ArtifactKind::Text, "b", "ceo", 1);
        let json = serde_json::to_string(&a).unwrap();
        assert!(!json.contains("stepSeq"));
        let back: ArtifactRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.latest().unwrap().step_seq, None);
    }
}
