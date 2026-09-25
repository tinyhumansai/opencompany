//! Content digest and trust tier for an installed skill.
//!
//! An install pins a snapshot: the shared library's `SKILL.md` is persisted
//! verbatim and a later library edit does not rewrite it. What the pin lacked
//! was a way to *check* it. [`skill_digest`] supplies that — it is the value a
//! stored document is measured against, so a copy edited after install is
//! detectable and the library's current entry can be compared without keeping
//! a second copy of either document.
//!
//! [`trust_tier`] turns [`SkillSource`] into the label an operator reads. It is
//! computed here rather than stored beside the delta, so no write path can
//! promote a skill into a tier it did not earn.
//!
//! [`drift`] is the comparison the two make possible: pinned digest against the
//! stored document, and pinned digest against the library's current entry.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::company::skill_file::{SkillDoc, render_skill_md};
use crate::ports::skills_state::{SkillInstall, SkillSource, SkillTier};

/// The lowercase-hex SHA-256 of a `SKILL.md` document.
///
/// Over the rendered document exactly as persisted — frontmatter and body —
/// because that whole string is what reaches the agent, and a digest over only
/// the body would call a rewritten `description` unchanged. The catalogue line
/// is built from the frontmatter, so frontmatter is prompt-bound too.
pub fn skill_digest(doc: &str) -> String {
    let digest = Sha256::digest(doc.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The trust label for a skill with this [`SkillSource`].
///
/// `from_baseline` distinguishes the two halves of [`SkillSource::Company`]:
/// the embedded global baseline every company gets, versus a skill committed
/// in this company's own bundle. The source alone cannot tell them apart, and
/// they are different trust stories — one is reviewed once for every host, the
/// other by whoever reviews that bundle. Callers supply it by asking
/// [`globals::skills`](crate::globals::skills) whether it ships the slug; it is
/// a parameter rather than a lookup so this stays a pure function of its
/// inputs and a test need not pin itself to whatever the baseline ships today.
///
/// It is ignored for the other sources: a registry install and a console-authored
/// skill are never baseline content whatever the baseline happens to contain.
pub fn trust_tier(source: SkillSource, from_baseline: bool) -> SkillTier {
    match source {
        SkillSource::Company if from_baseline => SkillTier::Builtin,
        SkillSource::Company => SkillTier::Company,
        SkillSource::Registry => SkillTier::Registry,
        SkillSource::Custom => SkillTier::Custom,
    }
}

/// The `version` frontmatter either side of a library change.
///
/// Both sides are optional because the field is optional, and neither is
/// ordered against the other: `version` is free text a publisher writes, so a
/// reader may say the document *changed*, never that it is *newer*. The digests
/// decided that something changed; these two strings only name the revisions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionChange {
    /// The `version` recorded when the install pinned its snapshot.
    pub from: Option<String>,
    /// The `version` the library's current document declares.
    pub to: Option<String>,
}

/// Where a pinned install stands: against the library, and against its own pin.
///
/// The two are independent and can both be true. A locally edited copy of a
/// skill whose library entry also moved is still one row, and collapsing it
/// into a single verdict would either hide the edit or hide the update.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDrift {
    /// Set when the library's current document differs from the pinned one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_available: Option<VersionChange>,
    /// Set when the stored document no longer matches the digest recorded at
    /// install: the installed copy was edited after it was pinned.
    pub modified: bool,
}

impl SkillDrift {
    /// Whether an update may apply the library's document over this install.
    ///
    /// False while `modified`, which is the point of recording the digest:
    /// overwriting would discard an operator's edit with no way to recover it,
    /// so the update refuses and leaves the choice with a human. Also false
    /// when nothing has changed, because then there is nothing to apply.
    pub fn update_allowed(&self) -> bool {
        self.update_available.is_some() && !self.modified
    }
}

/// Compares a pinned install against the document stored for it and against the
/// library's current entry for the same slug.
///
/// `stored_doc` is the `SKILL.md` persisted for this delta — what the agent
/// reads today. `library` is the library's entry now, or `None` when the slug
/// has left the library, which is not drift: an install keeps working from its
/// snapshot and there is no newer document to offer.
pub fn drift(install: &SkillInstall, stored_doc: &str, library: Option<&SkillDoc>) -> SkillDrift {
    let update_available = library
        .map(|doc| (doc, skill_digest(&render_skill_md(doc))))
        .filter(|(_, live)| *live != install.digest)
        .map(|(doc, _)| VersionChange {
            from: install.version.clone(),
            to: doc.version.clone(),
        });
    SkillDrift {
        update_available,
        modified: skill_digest(stored_doc) != install.digest,
    }
}

#[cfg(test)]
#[path = "skill_provenance_tests.rs"]
mod tests;
