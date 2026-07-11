//! Core feedback types: the [`FeedbackItem`] snapshot, its category, and the
//! per-company consent mode.
//!
//! A `FeedbackItem` is captured whenever an operator (or the brain on their
//! behalf) reacts to the company's work. It persists in the company's feedback
//! family whether or not it is ever filed as a public issue. The operator's
//! *unscrubbed* words stay local in the item; only a scrubbed candidate body
//! ever leaves the machine (see [`super::scrub`]).

use serde::{Deserialize, Serialize};

use crate::ports::generate_id;
use crate::ports::now_millis;

/// The kind of problem a feedback item reports.
///
/// Serialized in kebab-case so the wire form matches the `type/<value>` GitHub
/// label taxonomy in `docs/spec/feedback-loop/triage.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeedbackCategory {
    /// The company produced incorrect or low-quality work.
    WrongOutput,
    /// Runtime misbehavior: a crash, a lost event, a broken route.
    Bug,
    /// "It can't do X" — a capability the company lacks.
    MissingCapability,
    /// The approval fence is wrong: it over- or under-asks.
    ApprovalFriction,
    /// A template's roster, charter, or defaults fall short.
    TemplateGap,
    /// Documentation is wrong or missing.
    Docs,
}

impl FeedbackCategory {
    /// The kebab-case wire token used in the `type/` label and issue titles.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WrongOutput => "wrong-output",
            Self::Bug => "bug",
            Self::MissingCapability => "missing-capability",
            Self::ApprovalFriction => "approval-friction",
            Self::TemplateGap => "template-gap",
            Self::Docs => "docs",
        }
    }
}

/// How much standing consent the operator has granted for filing.
///
/// The scrub-then-preview gate (`docs/spec/feedback-loop/privacy.md`) applies in
/// every mode; the mode only decides whether the operator taps confirm each
/// time or has pre-authorized a category.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsentMode {
    /// Nothing leaves the machine automatically; the operator files via a
    /// prefilled issue link. The default.
    #[default]
    Manual,
    /// The company drafts the issue; the operator approves the exact final body.
    Assisted,
    /// Standing per-category consent; still scrubbed, previewed, and journaled.
    Auto,
}

/// The fields a capture surface supplies to mint a [`FeedbackItem`].
#[derive(Clone, Debug, Deserialize)]
pub struct FeedbackInput {
    /// The reported category.
    pub category: FeedbackCategory,
    /// The operator's own words. Kept local unscrubbed; scrubbed before filing.
    pub note: String,
    /// The work item the feedback concerns (an effect kind, route, or ref).
    #[serde(default)]
    pub work_ref: Option<String>,
    /// The template name the company was created from, if known.
    #[serde(default)]
    pub template_name: Option<String>,
    /// The template version the company was created from, if known.
    #[serde(default)]
    pub template_version: Option<String>,
}

/// A durable snapshot of one piece of feedback.
///
/// The `operator_words` field stays local and unscrubbed; a scrubbed candidate
/// body is derived from it only at filing time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedbackItem {
    /// A process-unique id.
    pub id: String,
    /// The reported category.
    pub category: FeedbackCategory,
    /// The operator's own words. **Local only** — never leaves unscrubbed.
    pub operator_words: String,
    /// The work item the feedback concerns.
    pub work_item: Option<String>,
    /// The template the company was created from, if known.
    pub template_name: Option<String>,
    /// The template version, if known.
    pub template_version: Option<String>,
    /// The runtime version that produced the work.
    pub runtime_version: String,
    /// A capped, scrubbed-at-filing excerpt of runtime/brain output.
    pub context_excerpt: String,
    /// Epoch-millis the item was captured.
    pub at_millis: u64,
    /// The filed issue URL, once the item is filed.
    pub filed_issue_url: Option<String>,
    /// The last-observed issue status, once the item is filed.
    pub issue_status: Option<String>,
    /// The consent mode in force when the item was captured.
    pub consent_mode: ConsentMode,
}

/// The maximum length of a captured context excerpt, in bytes. Data
/// minimization: issues carry the minimum needed to reproduce.
pub const CONTEXT_EXCERPT_CAP: usize = 2000;

impl FeedbackItem {
    /// Mints an item from a capture [`FeedbackInput`], stamping the current
    /// runtime version, time, and a fresh id. The context excerpt is capped.
    pub fn capture(input: FeedbackInput, runtime_version: &str, consent_mode: ConsentMode) -> Self {
        let mut excerpt = input.note.clone();
        if excerpt.len() > CONTEXT_EXCERPT_CAP {
            // Truncate on a UTF-8 char boundary at or below the cap so a
            // multi-byte character straddling the cap never panics.
            let mut end = CONTEXT_EXCERPT_CAP;
            while end > 0 && !excerpt.is_char_boundary(end) {
                end -= 1;
            }
            excerpt.truncate(end);
        }
        Self {
            id: generate_id(),
            category: input.category,
            operator_words: input.note,
            work_item: input.work_ref,
            template_name: input.template_name,
            template_version: input.template_version,
            runtime_version: runtime_version.to_string(),
            context_excerpt: excerpt,
            at_millis: now_millis(),
            filed_issue_url: None,
            issue_status: None,
            consent_mode,
        }
    }
}

/// Detects a feedback intent in an operator chat message.
///
/// Recognizes a small set of complaint phrases ("that was wrong", "flag it",
/// "this is a bug", "it can't") so an operator can file feedback mid-chat
/// without a separate call. Returns the inferred category, or `None` when the
/// message carries no feedback intent (the common case, so normal chat is
/// untouched). Conservative by design: it never fires on neutral messages.
pub fn detect_chat_intent(text: &str) -> Option<FeedbackCategory> {
    let lower = text.to_lowercase();
    // Ordered most-specific first so a message matches its truest category.
    const RULES: &[(&[&str], FeedbackCategory)] = &[
        (
            &["can't do", "cannot do", "can it ", "unable to"],
            FeedbackCategory::MissingCapability,
        ),
        (
            &["asked me again", "why approve", "too many approvals"],
            FeedbackCategory::ApprovalFriction,
        ),
        (
            &["crashed", "is a bug", "broken", "lost my"],
            FeedbackCategory::Bug,
        ),
        (
            &["docs are", "documentation is", "docs wrong"],
            FeedbackCategory::Docs,
        ),
        (
            &[
                "was wrong",
                "is wrong",
                "flag it",
                "flag this",
                "this was wrong",
                "that was wrong",
                "thumbs down",
            ],
            FeedbackCategory::WrongOutput,
        ),
    ];
    for (needles, category) in RULES {
        if needles.iter().any(|needle| lower.contains(needle)) {
            return Some(*category);
        }
    }
    None
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn detects_complaint_intents_only() {
        assert_eq!(
            detect_chat_intent("that invoice was wrong — flag it"),
            Some(FeedbackCategory::WrongOutput)
        );
        assert_eq!(
            detect_chat_intent("it can't do multi-currency"),
            Some(FeedbackCategory::MissingCapability)
        );
        assert_eq!(
            detect_chat_intent("the server crashed"),
            Some(FeedbackCategory::Bug)
        );
        // Neutral chat carries no intent.
        assert_eq!(detect_chat_intent("hi"), None);
        assert_eq!(detect_chat_intent("file it under Q3"), None);
    }

    #[test]
    fn category_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&FeedbackCategory::WrongOutput).unwrap(),
            "\"wrong-output\""
        );
        assert_eq!(
            FeedbackCategory::MissingCapability.as_str(),
            "missing-capability"
        );
    }

    #[test]
    fn consent_defaults_to_manual() {
        assert_eq!(ConsentMode::default(), ConsentMode::Manual);
        assert_eq!(
            serde_json::to_string(&ConsentMode::Auto).unwrap(),
            "\"auto\""
        );
    }

    #[test]
    fn capture_stamps_version_and_caps_excerpt() {
        let input = FeedbackInput {
            category: FeedbackCategory::Bug,
            note: "x".repeat(CONTEXT_EXCERPT_CAP + 500),
            work_ref: Some("email.send".into()),
            template_name: None,
            template_version: None,
        };
        let item = FeedbackItem::capture(input, "9.9.9", ConsentMode::Manual);
        assert_eq!(item.runtime_version, "9.9.9");
        assert_eq!(item.context_excerpt.len(), CONTEXT_EXCERPT_CAP);
        // The full operator words stay local, uncapped.
        assert_eq!(item.operator_words.len(), CONTEXT_EXCERPT_CAP + 500);
        assert_eq!(item.work_item.as_deref(), Some("email.send"));
    }

    #[test]
    fn capture_caps_multibyte_excerpt_on_a_char_boundary() {
        // "😀" is 4 bytes; a run of them makes byte CONTEXT_EXCERPT_CAP land
        // mid-character, which would panic a naive String::truncate.
        let input = FeedbackInput {
            category: FeedbackCategory::Bug,
            note: "😀".repeat(CONTEXT_EXCERPT_CAP),
            work_ref: None,
            template_name: None,
            template_version: None,
        };
        let item = FeedbackItem::capture(input, "9.9.9", ConsentMode::Manual);
        assert!(item.context_excerpt.len() <= CONTEXT_EXCERPT_CAP);
        // Truncated on a boundary: the excerpt is still valid UTF-8 emoji.
        assert!(item.context_excerpt.chars().all(|c| c == '😀'));
    }

    #[test]
    fn item_round_trips_through_json() {
        let item = FeedbackItem::capture(
            FeedbackInput {
                category: FeedbackCategory::TemplateGap,
                note: "roster too thin".into(),
                work_ref: None,
                template_name: Some("marketing_agency".into()),
                template_version: Some("1.2".into()),
            },
            "0.1.0",
            ConsentMode::Auto,
        );
        let json = serde_json::to_string(&item).unwrap();
        let back: FeedbackItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back, item);
    }
}
