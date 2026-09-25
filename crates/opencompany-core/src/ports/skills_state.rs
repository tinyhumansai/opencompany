//! The [`SkillStateStore`] port: operator deltas over the company's skills.
//!
//! Built-in skill content lives on disk (`companies/<name>/skills/**`, whose
//! union across bundles is the registry). This store holds only the **deltas** the operator
//! applies through the console: library installs, custom skills authored
//! in-app, and enable/disable overrides. The effective skill set is the
//! company-dir skills unioned with these rows (see the seeder in
//! [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder::build)).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// Where a skill came from. Mirrors the console's `SkillSource`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// When this delta was last written, in epoch milliseconds.
    ///
    /// `None` for a row stored before the field existed, and for the
    /// disabling deltas a manifest's `[globals].disable` synthesizes — neither
    /// was ever edited by anyone, and a fabricated stamp would date them to
    /// whenever the process happened to read the manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_millis: Option<u64>,
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

#[cfg(test)]
#[path = "skills_state_tests.rs"]
mod tests;
