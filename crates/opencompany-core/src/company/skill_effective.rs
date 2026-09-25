//! A company's effective skill set — the one derivation every surface reads.
//!
//! Three things need to know which skills a company has: the harness, which
//! materializes each agent's `skills/<slug>/` tree; `GET …/skills`, which the
//! console's Skills tab renders; and the GraphQL `Company.skills` resolver.
//! [`resolve`] is what all three read, so the console cannot report a set the
//! agents do not have.
//!
//! The layers, bottom to top:
//!
//! 1. the global baseline ([`crate::globals::skills`]), installed in every
//!    company — including a platform-provisioned tenant with no checkout;
//! 2. the company's committed bundles (`companies/<name>/skills/*/SKILL.md`);
//! 3. the operator's [`SkillState`] deltas — an enable/disable override, a
//!    registry install's pinned snapshot, or a console-authored skill.
//!
//! A company's `[globals].disable = ["skill:…"]` enters as a synthesized
//! disabling delta ([`globals_skill_disables`]) rather than as a second opt-out
//! mechanism, so the manifest and the console say the same thing in the same
//! vocabulary — and a disable beats an enable, so a company's own declaration
//! survives a console re-enable.
//!
//! [`resolve`] reports **disabled entries too**, which is what separates it
//! from what the harness writes to disk: the console needs the row to render
//! the switch that turns the skill back on. The harness filters to the enabled
//! entries.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::company::{SkillDoc, load_dir_skills, parse_skill_md, render_skill_md};
use crate::error::Result;
use crate::ports::skills_state::{SkillSource, SkillState};

/// Where an effective skill's `SKILL.md` comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum SkillBody {
    /// A company-dir bundle directory, copied verbatim so its bundled resource
    /// files travel with the document.
    Bundle(PathBuf),
    /// A rendered `SKILL.md`, written inline. A global and a console-authored
    /// skill both arrive this way — neither has a directory to copy.
    Inline(String),
}

/// A resolved skill document and the `SKILL.md` behind it.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillContent {
    pub doc: SkillDoc,
    pub body: SkillBody,
}

/// One slug in a company's effective skill set.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveSkill {
    pub slug: String,
    /// Whether an agent actually gets this skill.
    pub enabled: bool,
    pub source: SkillSource,
    /// `None` when no layer supplies a document — an enable-only delta over a
    /// slug that has no bundle, no global, and no snapshot. Such a row reaches
    /// no agent, and the readers render it from its slug alone.
    pub content: Option<SkillContent>,
}

impl EffectiveSkill {
    /// The resolved document, when a layer supplied one.
    pub fn doc(&self) -> Option<&SkillDoc> {
        self.content.as_ref().map(|content| &content.doc)
    }
}

/// Whether `slug` is a safe skill id: `^[a-z0-9][a-z0-9-]*$`.
///
/// A slug is also a directory name in an agent's scratch tree
/// (`skills/<slug>/`), so a traversal (`..`) or a path separator would escape
/// it via [`Path::join`]. Refused wherever a slug enters.
pub fn valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The disabling [`SkillState`] deltas a company's `[globals].disable` implies.
///
/// One per `skill:<slug>` entry, and nothing else: an entry naming another kind
/// is that kind's business, and manifest validation has already refused an entry
/// naming nothing at all.
pub fn globals_skill_disables(disable: &[String]) -> Vec<SkillState> {
    disable
        .iter()
        .filter_map(|entry| entry.strip_prefix("skill:"))
        .map(|slug| SkillState {
            slug: slug.to_string(),
            enabled: false,
            // `[globals].disable` can only name a global, and a global is a
            // baseline install rather than something an operator added — the
            // same provenance [`resolve`] gives it, so the synthesized delta
            // reports the skill the way it would have been reported unopposed.
            source: SkillSource::Company,
            custom_doc: None,
            install: None,
        })
        .collect()
}

/// Resolves a company's effective skill set, ordered by slug.
///
/// `source_dir` is the company's source directory (`companies/<name>`), `None`
/// for a platform-provisioned tenant. `registry` is the repo-level shared
/// library, read only to heal a degenerate registry snapshot (see
/// [`registry_heal`]). `deltas` are the store's rows plus whatever
/// [`globals_skill_disables`] adds for the manifest.
///
/// Resolution rules:
/// * the global baseline is the bottom layer, superseded by any same-slug
///   company bundle or delta snapshot and dropped by a disabling delta;
/// * a company bundle is enabled unless a delta disables it;
/// * a delta sets the slug's `enabled` flag and its provenance, and a delta
///   carrying a `custom_doc` supersedes the document beneath it;
/// * a delta with no `custom_doc` contributes no document of its own;
/// * a disable anywhere in `deltas` wins over an enable;
/// * a malformed `custom_doc`, or a delta whose slug is not a safe directory
///   name, contributes nothing rather than failing the resolution.
///
/// A malformed company bundle **does** fail it: `load_dir_skills` refuses the
/// whole directory, so every agent in that company loses its catalogue, and a
/// reader that answered with the surviving subset would describe a set no agent
/// has.
pub fn resolve(
    source_dir: Option<&Path>,
    registry: &[SkillDoc],
    deltas: &[SkillState],
) -> Result<Vec<EffectiveSkill>> {
    let mut entries: BTreeMap<String, EffectiveSkill> = BTreeMap::new();

    for doc in crate::globals::skills() {
        entries.insert(
            doc.slug.clone(),
            EffectiveSkill {
                slug: doc.slug.clone(),
                enabled: true,
                source: SkillSource::Company,
                content: Some(SkillContent {
                    doc: doc.clone(),
                    body: SkillBody::Inline(render_skill_md(doc)),
                }),
            },
        );
    }

    if let Some(dir) = source_dir {
        let root = dir.join("skills");
        for doc in load_dir_skills(&root)? {
            let slug = doc.slug.clone();
            let bundle = root.join(&slug);
            entries.insert(
                slug.clone(),
                EffectiveSkill {
                    slug,
                    enabled: true,
                    source: SkillSource::Company,
                    content: Some(SkillContent {
                        doc,
                        body: SkillBody::Bundle(bundle),
                    }),
                },
            );
        }
    }

    let mut disabled: HashSet<&str> = HashSet::new();
    for delta in deltas {
        if !valid_slug(&delta.slug) {
            tracing::warn!(
                "[skills] skipping a skill delta whose slug is not a safe directory name: {:?}",
                delta.slug
            );
            continue;
        }
        if !delta.enabled {
            disabled.insert(delta.slug.as_str());
        }
        let entry = entries
            .entry(delta.slug.clone())
            .or_insert_with(|| EffectiveSkill {
                slug: delta.slug.clone(),
                enabled: delta.enabled,
                source: delta.source,
                content: None,
            });
        entry.enabled = delta.enabled;
        entry.source = delta.source;
        if let Some(content) = delta_content(delta, registry) {
            entry.content = Some(content);
        }
    }

    for entry in entries.values_mut() {
        if disabled.contains(entry.slug.as_str()) {
            entry.enabled = false;
        }
    }

    Ok(entries.into_values().collect())
}

/// The document a delta contributes, or `None` when it contributes none.
fn delta_content(delta: &SkillState, registry: &[SkillDoc]) -> Option<SkillContent> {
    let src = delta.custom_doc.as_deref()?;
    let parsed = parse_skill_md(&delta.slug, src);
    if let Some(live) = registry_heal(delta, parsed.as_ref().ok(), registry) {
        tracing::info!(
            "[skills] healing pre-fix registry install '{}' from the shared library",
            delta.slug
        );
        return Some(SkillContent {
            doc: live.clone(),
            body: SkillBody::Inline(render_skill_md(live)),
        });
    }
    match parsed {
        Ok(doc) => Some(SkillContent {
            doc,
            body: SkillBody::Inline(src.to_string()),
        }),
        Err(err) => {
            tracing::warn!(
                "[skills] skipping malformed custom skill '{}': {err}",
                delta.slug
            );
            None
        }
    }
}

/// Whether a stored `SKILL.md` snapshot is a **pre-fix registry stub**.
///
/// Before this was fixed, installing a registry skill persisted a document built
/// from the client's metadata with the description doubling as the body, so the
/// agent read a one-line summary instead of the procedure. Such a snapshot is
/// recognisable by construction: its body is exactly its own description.
///
/// A legitimately one-line skill (body identical to its description) would also
/// match, and would be re-served from the live library rather than from its
/// pinned snapshot. That is the one honest false positive: it costs the pin, not
/// the content, and only for a skill whose entire body is a single line already
/// held verbatim in its own frontmatter. No skill in the shared library is
/// shaped that way (a test pins that), so it is a hypothetical.
fn is_registry_stub(doc: &SkillDoc) -> bool {
    doc.body.trim() == doc.description.trim()
}

/// The live library document that should supersede a stored snapshot, or `None`
/// to keep whatever the row has.
///
/// `stored` is the parsed snapshot, or `None` when it does not parse at all —
/// which the pre-fix path could produce, since it wrote `description:` with an
/// empty value when the client sent no description, and the parser rejects that.
/// Such a row would otherwise be dropped from the effective set entirely, so
/// healing it turns a silently missing skill into a working one.
///
/// Scoped deliberately narrowly:
///
/// * **Only `Registry`-sourced rows.** A `Custom` row is operator-authored and a
///   `Company` row is committed to the repo; neither is ever second-guessed, so
///   the heal cannot clobber content a human wrote. There is no route that
///   writes an operator-authored body onto a `Registry` row — `install` upserts
///   a snapshot and `set_enabled` only carries the existing doc forward — so a
///   `Registry` body is always machine-generated.
/// * **Only a degenerate or unparseable snapshot.** A real snapshot is left
///   pinned, so an install does not silently track later library edits.
/// * **Only when the slug is in the library**, so an install of a skill that has
///   since left it keeps whatever it has rather than vanishing.
fn registry_heal<'a>(
    delta: &SkillState,
    stored: Option<&SkillDoc>,
    registry: &'a [SkillDoc],
) -> Option<&'a SkillDoc> {
    if delta.source != SkillSource::Registry {
        return None;
    }
    if stored.is_some_and(|doc| !is_registry_stub(doc)) {
        return None;
    }
    registry.iter().find(|doc| doc.slug == delta.slug)
}

#[cfg(test)]
#[path = "skill_effective/skill_effective_tests.rs"]
mod tests;
