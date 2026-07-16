//! The [`UsageMeter`] port: durable per-company usage samples.
//!
//! Every metered event — a model inference turn, or an OAuth-connected tool
//! call — is recorded as one [`UsageSample`]. The WS4 cost hook writes samples
//! here; the WS5 Usage/Finances reads aggregate them (`query` returns the
//! window a console chart renders). Samples are non-secret accounting rows;
//! money still resolves from the ledger and `[budget]`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// How long a [`UsageMeter`] backend retains samples: the console's maximum
/// window (`UsageRange::D90`). Backends evict samples older than this on write.
pub const RETENTION_DAYS: u64 = 90;

/// [`RETENTION_DAYS`] expressed in milliseconds.
pub const RETENTION_MILLIS: u64 = RETENTION_DAYS * 86_400_000;

/// The oldest `at_millis` a backend keeps, given the newest sample it has seen
/// (typically ~now). Samples strictly older than this are evicted; a sample
/// exactly [`RETENTION_DAYS`] old is still inside the window and kept.
///
/// Anchoring to the newest observed sample (rather than wall-clock now) keeps
/// eviction deterministic and testable, and never discards a company's only
/// recent data just because the process clock moved.
pub fn retention_cutoff(newest_at_millis: u64) -> u64 {
    newest_at_millis.saturating_sub(RETENTION_MILLIS)
}

/// What produced a [`UsageSample`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SampleKind {
    /// Tokens consumed by a model inference call.
    Inference,
    /// An OAuth-connected tool invocation (populates the calls-by-provider
    /// chart). Wired by the runtime when a connected tool runs.
    OauthCall,
}

/// One metered usage event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSample {
    /// Epoch-millis timestamp the event happened.
    pub at_millis: u64,
    /// The agent that produced the usage.
    pub agent: String,
    /// The inference/tool provider slug (e.g. `managed`, `github`).
    pub provider: String,
    /// Input/prompt tokens consumed.
    pub input_tokens: u64,
    /// Output/completion tokens produced.
    pub output_tokens: u64,
    /// Input tokens served from the KV cache.
    pub cached_input_tokens: u64,
    /// USD cost attributed to the sample.
    pub cost_usd: f64,
    /// What produced the sample.
    pub kind: SampleKind,
}

/// Durable per-company usage samples. Company A's usage MUST be invisible to
/// company B.
#[async_trait]
pub trait UsageMeter: Send + Sync {
    /// Records a single usage sample.
    async fn record(&self, company: &CompanyId, sample: &UsageSample) -> Result<()>;
    /// Returns every sample at or after `since_millis`, oldest first.
    async fn query(&self, company: &CompanyId, since_millis: u64) -> Result<Vec<UsageSample>>;
}
