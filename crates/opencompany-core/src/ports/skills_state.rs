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
use crate::ports::types::{Actor, CompanyId};

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

/// The trust label a console row shows against a skill.
///
/// Computed from [`SkillSource`] plus one bit the source cannot carry — whether
/// a company skill came from the embedded global baseline or from the company's
/// own bundle — and never stored, so an operator cannot edit a skill into a
/// tier it did not earn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillTier {
    /// The embedded global baseline every company gets (`companies/_globals/skills`).
    Builtin,
    /// Committed in this company's own bundle (`companies/<name>/skills`).
    Company,
    /// Installed from the shared registry against a pinned snapshot.
    Registry,
    /// Authored or uploaded in the console, including the empty-registry
    /// install fallback, whose document the client wrote.
    Custom,
}

/// What an install pinned: the exact document, the publisher version it
/// claimed, and who pinned it when.
///
/// The pin itself already exists — the install persists the library's document
/// verbatim and a later library edit does not rewrite it. This is the part that
/// makes the pin *checkable*: `digest` is what "unchanged" is measured against,
/// so an installed copy that was edited afterwards is detectable and an update
/// can refuse rather than discard the edit.
///
/// Absent on a delta that installed nothing — an enable/disable override over a
/// skill the bundle already ships — and on every row written before provenance
/// was recorded, which is why [`SkillState::install`] is an `Option` carrying
/// `#[serde(default)]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInstall {
    /// Lowercase-hex SHA-256 of the `SKILL.md` this install persisted.
    pub digest: String,
    /// The pinned document's `version` frontmatter, when it declared one.
    ///
    /// Free text with no ordering — it names which revision was pinned, and a
    /// comparison against the library may only report *changed*, never *newer*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Who installed it, when known. `None` from a surface that carries no
    /// attributed actor, the same shape every journalled `by` uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_by: Option<Actor>,
    /// Epoch-millis the install landed, matching
    /// [`StoredEvent::at_millis`](crate::ports::types::StoredEvent::at_millis).
    pub installed_at_millis: u64,
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
    /// What this delta's install pinned, when it installed anything.
    ///
    /// Additive: a row written before provenance existed carries no `install`
    /// key and deserializes to `None`, and an exported bundle from such a host
    /// imports unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<SkillInstall>,
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
