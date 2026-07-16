//! The [`SkillStateStore`] port: operator deltas over the company's skills.
//!
//! Built-in skill content lives on disk (`companies/<name>/skills/**` and the
//! repo-level registry). This store holds only the **deltas** the operator
//! applies through the console: library installs, custom skills authored
//! in-app, and enable/disable overrides. The effective skill set is the
//! company-dir skills unioned with these rows (see the seeder in
//! [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder::build)).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// Where a skill came from. Mirrors the console's `SkillSource`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    /// A skill shipped in the company bundle (`companies/<name>/skills/**`).
    Company,
    /// A skill installed from the shared registry.
    Registry,
    /// A custom skill the operator authored in the console.
    Custom,
}

/// One operator delta over a skill, keyed by slug.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillState {
    /// The skill's slug (its stable id).
    pub slug: String,
    /// Whether the skill is enabled.
    pub enabled: bool,
    /// Where the skill came from.
    pub source: SkillSource,
    /// The full `SKILL.md` document for a custom skill; `None` for a delta over
    /// a built-in or registry skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_doc: Option<String>,
}

/// Durable per-company skill deltas. Company A's deltas MUST be invisible to
/// company B.
#[async_trait]
pub trait SkillStateStore: Send + Sync {
    /// Lists every delta.
    async fn list(&self, company: &CompanyId) -> Result<Vec<SkillState>>;
    /// Inserts or replaces a delta by slug.
    async fn set(&self, company: &CompanyId, state: &SkillState) -> Result<()>;
    /// Removes a delta by slug; returns whether one was removed.
    async fn remove(&self, company: &CompanyId, slug: &str) -> Result<bool>;
}
