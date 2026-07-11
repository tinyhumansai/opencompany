//! The feedback loop: capture → scrub → preview → file.
//!
//! In-product reactions become durable [`FeedbackItem`]s
//! ([`types`]), persisted in the company's feedback family
//! ([`store`]). Filing to the public tracker passes through the normative
//! privacy [`scrub`]ber and a scrub-then-preview gate before a mockable
//! [`github`] client files (or degrades to a manual link). [`service`] wires the
//! whole flow; [`tool`] exposes a built-in `feedback` tool the brain can invoke;
//! [`labels`] maps categories to the triage taxonomy.
//!
//! The trait + mock transport and the scrubber/store compile in the default
//! build so the loop is exercised entirely offline; only the real HTTP filer is
//! gated behind the `github` feature.

pub mod github;
pub mod labels;
pub mod scrub;
pub mod service;
pub mod store;
pub mod tool;
pub mod types;

pub use github::{
    FilingOutcome, GitHubClient, IssueDraft, MockGitHubClient, RateLimiter, file_feedback,
    manual_issue_url, sign_body,
};
pub use scrub::{CharterTerm, ScrubOutcome, scrub};
pub use service::{FeedbackFiler, FeedbackResponse};
pub use store::FeedbackStore;
pub use tool::BuiltinToolProvider;
pub use types::{ConsentMode, FeedbackCategory, FeedbackInput, FeedbackItem, detect_chat_intent};

#[cfg(feature = "github")]
pub use github::HttpGitHubClient;

/// The public issue tracker feedback is filed against.
pub const DEFAULT_REPO: &str = "tinyhumansai/opencompany";
