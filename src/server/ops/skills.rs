//! Skill writes: install/uninstall a registry skill, toggle enabled, and author
//! a custom skill — under both scope forms.
//!
//! Deltas land in the [`SkillStateStore`](crate::ports::SkillStateStore); the
//! built-in skill content stays on disk (seeded by
//! [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder)). The `InstalledSkill`
//! response mirrors the console (`frontend/src/lib/skills.ts`); for registry /
//! built-in skills the name/description are best-effort (the console enriches
//! from its catalog), while a custom skill's fields come from its `SKILL.md`.

use std::collections::HashMap;
use std::path::Path as FsPath;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::{SkillDoc, parse_skill_md};
use crate::error::OpenCompanyError;
use crate::ports::skills_state::{SkillSource, SkillState};
use crate::server::error::ApiError;
use crate::server::ops::language;
use crate::server::ops::{ScopedCompany, scoped};

/// The default category stamped on a skill whose doc carries none.
const DEFAULT_CATEGORY: &str = "Ops";

/// Builds the skills route fragment.
pub fn router() -> Router<AppState> {
    scoped("/skills/{slug}/install", post(install))
        .merge(scoped("/skills/{slug}/uninstall", post(uninstall)))
        .merge(scoped("/skills/{slug}", put(set_enabled)))
        .merge(scoped("/skills", post(create_custom).get(list_skills)))
}

/// An installed skill as the console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstalledSkill {
    id: String,
    name: String,
    description: String,
    category: String,
    source: SkillSource,
    enabled: bool,
}

impl InstalledSkill {
    /// Projects a [`SkillState`] to the console shape, parsing a custom skill's
    /// `SKILL.md` for its name/description/category and falling back to a
    /// slug-derived name for registry/built-in deltas.
    fn from_state(state: &SkillState) -> Self {
        let (name, description, category) = match &state.custom_doc {
            Some(doc) => match parse_skill_md(&state.slug, doc) {
                Ok(parsed) => (
                    parsed.name,
                    parsed.description,
                    parsed
                        .category
                        .unwrap_or_else(|| DEFAULT_CATEGORY.to_string()),
                ),
                Err(_) => (
                    titleize(&state.slug),
                    String::new(),
                    DEFAULT_CATEGORY.to_string(),
                ),
            },
            None => (
                titleize(&state.slug),
                String::new(),
                DEFAULT_CATEGORY.to_string(),
            ),
        };
        Self {
            id: state.slug.clone(),
            name,
            description,
            category,
            source: state.source,
            enabled: state.enabled,
        }
    }

    /// Projects a company-bundle `SKILL.md` (`companies/<name>/skills/<slug>`)
    /// to the console shape. These are [`SkillSource::Company`], enabled unless
    /// a store delta later overrides the flag.
    fn from_company_bundle(doc: &SkillDoc, enabled: bool) -> Self {
        Self {
            id: doc.slug.clone(),
            name: doc.name.clone(),
            description: doc.description.clone(),
            category: doc
                .category
                .clone()
                .unwrap_or_else(|| DEFAULT_CATEGORY.to_string()),
            source: SkillSource::Company,
            enabled,
        }
    }
}

/// The sub-resource path (`slug`).
#[derive(Debug, Deserialize)]
struct SlugPath {
    slug: String,
}

/// The toggle body.
#[derive(Debug, Deserialize)]
struct SetEnabled {
    enabled: bool,
}

/// The install body — the registry entry's metadata, so the installed skill
/// carries a real `SKILL.md` the embedded agent can act on (a bare slug has no
/// content, so it would never reach the agent's effective set).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallSkill {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    category: Option<String>,
}

/// The custom-skill body.
#[derive(Debug, Deserialize)]
struct CreateSkill {
    name: String,
    description: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// `GET …/skills` — the company's **effective** skill set: its on-disk bundles
/// (`companies/<name>/skills/*/SKILL.md`) unioned with the operator's
/// [`SkillStateStore`] deltas. The console renders this list; it mirrors the
/// write-plane semantics (and the GraphQL `Company.skills` resolver).
async fn list_skills(company: ScopedCompany) -> Result<Json<Vec<InstalledSkill>>, ApiError> {
    let deltas = company.runtime.skills().list(company.id()).await?;
    Ok(Json(merge_effective(company.runtime.source_dir(), deltas)))
}

/// Merges the company-dir bundles with the operator deltas: a delta over a
/// same-slug bundle wins its `enabled` flag, source, and (if it carries one) its
/// custom doc; a delta with no bundle appears on its own. Sorted by slug so the
/// response is deterministic.
fn merge_effective(source_dir: Option<&FsPath>, deltas: Vec<SkillState>) -> Vec<InstalledSkill> {
    let mut by_slug: HashMap<String, InstalledSkill> = company_bundles(source_dir)
        .into_iter()
        .map(|skill| (skill.id.clone(), skill))
        .collect();

    for st in deltas {
        match by_slug.get_mut(&st.slug) {
            Some(existing) => {
                existing.enabled = st.enabled;
                existing.source = st.source;
                // A delta that carries a doc (a custom override) refreshes the
                // display fields; a plain enable/disable delta keeps the bundle's.
                if let Some(doc) = st
                    .custom_doc
                    .as_deref()
                    .and_then(|doc| parse_skill_md(&st.slug, doc).ok())
                {
                    existing.name = doc.name;
                    existing.description = doc.description;
                    existing.category =
                        doc.category.unwrap_or_else(|| DEFAULT_CATEGORY.to_string());
                }
            }
            None => {
                by_slug.insert(st.slug.clone(), InstalledSkill::from_state(&st));
            }
        }
    }

    let mut out: Vec<InstalledSkill> = by_slug.into_values().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Scans `<source_dir>/skills/*/SKILL.md` into console skills. A missing source
/// dir (platform-provisioned mode) or unreadable directory yields an empty list,
/// and a missing or malformed `SKILL.md` skips just that bundle — never fails.
fn company_bundles(source_dir: Option<&FsPath>) -> Vec<InstalledSkill> {
    let Some(dir) = source_dir.map(|dir| dir.join("skills")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(slug) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(path.join("SKILL.md")) else {
            continue;
        };
        if let Ok(doc) = parse_skill_md(slug, &body) {
            out.push(InstalledSkill::from_company_bundle(&doc, true));
        }
    }
    out
}

async fn install(
    company: ScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
    body: Option<Json<InstallSkill>>,
) -> Result<Json<InstalledSkill>, ApiError> {
    let meta = body.map(|Json(b)| b).unwrap_or_default();
    let name = meta
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| titleize(&slug));
    let description = meta.description.unwrap_or_default();
    // The registry ships metadata only. Persist a real `SKILL.md` built from it
    // (the description doubles as the body) so `EffectiveSkills::materialize`
    // surfaces the skill to the agent instead of skipping a content-less delta.
    let doc = skill_md(&name, &description, meta.category.as_deref(), &description);
    let state = SkillState {
        slug,
        enabled: true,
        source: SkillSource::Registry,
        custom_doc: Some(doc),
    };
    company.runtime.skills().set(company.id(), &state).await?;
    Ok(Json(InstalledSkill::from_state(&state)))
}

async fn uninstall(
    company: ScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
) -> Result<StatusCode, ApiError> {
    let existing = company
        .runtime
        .skills()
        .list(company.id())
        .await?
        .into_iter()
        .find(|s| s.slug == slug);
    match existing {
        // Only registry installs and custom skills can be uninstalled.
        Some(state) if matches!(state.source, SkillSource::Registry | SkillSource::Custom) => {
            company.runtime.skills().remove(company.id(), &slug).await?;
            Ok(StatusCode::NO_CONTENT)
        }
        // A built-in (company) skill — with or without a delta row — cannot be
        // removed; it can only be disabled.
        _ => Err(ApiError(OpenCompanyError::Conflict(
            language::BUILTIN_UNINSTALL.to_string(),
        ))),
    }
}

async fn set_enabled(
    company: ScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
    Json(body): Json<SetEnabled>,
) -> Result<Json<InstalledSkill>, ApiError> {
    // Preserve an existing delta's source and custom doc; a first toggle of a
    // built-in company skill records a Company-sourced override.
    let existing = company
        .runtime
        .skills()
        .list(company.id())
        .await?
        .into_iter()
        .find(|s| s.slug == slug);
    let state = SkillState {
        slug,
        enabled: body.enabled,
        source: existing
            .as_ref()
            .map(|s| s.source)
            .unwrap_or(SkillSource::Company),
        custom_doc: existing.and_then(|s| s.custom_doc),
    };
    company.runtime.skills().set(company.id(), &state).await?;
    Ok(Json(InstalledSkill::from_state(&state)))
}

async fn create_custom(
    company: ScopedCompany,
    Json(body): Json<CreateSkill>,
) -> Result<Json<InstalledSkill>, ApiError> {
    if body.name.trim().is_empty() || body.description.trim().is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            language::SKILL_FIELDS_REQUIRED.to_string(),
        )));
    }
    let slug = slugify(&body.name);
    let doc = skill_md(
        &body.name,
        &body.description,
        body.category.as_deref(),
        body.body.as_deref().unwrap_or(""),
    );
    let state = SkillState {
        slug,
        enabled: true,
        source: SkillSource::Custom,
        custom_doc: Some(doc),
    };
    company.runtime.skills().set(company.id(), &state).await?;
    Ok(Json(InstalledSkill::from_state(&state)))
}

/// Builds a `SKILL.md` document from a name, description, optional category, and
/// body. Shared by custom-skill authoring and registry install (which passes
/// the description as the body).
///
/// The frontmatter parser is line-based (`key: value`), so each scalar is
/// collapsed to a single line: newlines become spaces. That prevents a
/// name/description from injecting extra frontmatter fields or emitting a bare
/// `---` line that would close the block early. (Colons within a value are
/// safe — the parser splits only on the first one.)
fn skill_md(name: &str, description: &str, category: Option<&str>, content: &str) -> String {
    let one_line = |s: &str| s.replace(['\n', '\r'], " ");
    let mut frontmatter = format!(
        "name: {}\ndescription: {}\n",
        one_line(name).trim(),
        one_line(description).trim()
    );
    if let Some(category) = category {
        frontmatter.push_str(&format!("category: {}\n", one_line(category).trim()));
    }
    format!("---\n{frontmatter}---\n{content}\n")
}

/// Turns a display name into a filesystem-and-URL-safe slug.
fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

/// Turns a slug into a human title (`web-research` → `Web Research`).
fn titleize(slug: &str) -> String {
    slug.split('-')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bundle(root: &FsPath, slug: &str, contents: &str) {
        let dir = root.join("skills").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), contents).unwrap();
    }

    #[test]
    fn skill_md_frontmatter_resists_injection() {
        // A name carrying newlines, a stray `---`, and a fake field must not
        // inject frontmatter or hijack another field: newlines collapse to
        // spaces, so it all lands as the single `name` value.
        let nasty_name = "Evil\n---\ninjected: true\nname: hijacked";
        let doc = skill_md(nasty_name, "a real description", Some("Ops"), "body");
        let parsed = parse_skill_md("evil", &doc).expect("frontmatter stays valid");
        assert_eq!(parsed.name, "Evil --- injected: true name: hijacked");
        // The description was NOT overwritten by the injected `name: hijacked`.
        assert_eq!(parsed.description, "a real description");
        // A colon inside a value is preserved (split only on the first colon).
        let colon = skill_md("Name", "ratio 3:1 outcome", None, "body");
        assert_eq!(
            parse_skill_md("c", &colon).unwrap().description,
            "ratio 3:1 outcome"
        );
    }

    #[test]
    fn merge_unions_bundles_with_deltas_and_skips_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A well-formed company bundle, plus one with no frontmatter that must be
        // skipped rather than failing the whole scan.
        write_bundle(
            root,
            "onboard",
            "---\nname: Onboard\ndescription: Get set up\ncategory: Ops\n---\n# Onboard\n",
        );
        write_bundle(root, "broken", "no frontmatter here\n");

        let deltas = vec![
            // Disables the company bundle above (a plain enable/disable delta).
            SkillState {
                slug: "onboard".to_string(),
                enabled: false,
                source: SkillSource::Company,
                custom_doc: None,
            },
            // A custom skill with no matching company bundle.
            SkillState {
                slug: "my-skill".to_string(),
                enabled: true,
                source: SkillSource::Custom,
                custom_doc: Some(
                    "---\nname: My Skill\ndescription: Does a thing\n---\n# body\n".to_string(),
                ),
            },
        ];

        let out = merge_effective(Some(root), deltas);

        // The company bundle appears, keeps its parsed name, and the delta flips
        // it disabled.
        let onboard = out
            .iter()
            .find(|s| s.id == "onboard")
            .expect("company bundle present");
        assert_eq!(onboard.name, "Onboard");
        assert_eq!(onboard.source, SkillSource::Company);
        assert!(!onboard.enabled, "delta flips the bundle disabled");

        // The custom delta appears on its own, enriched from its doc.
        let custom = out
            .iter()
            .find(|s| s.id == "my-skill")
            .expect("custom delta present");
        assert_eq!(custom.source, SkillSource::Custom);
        assert_eq!(custom.name, "My Skill");
        assert!(custom.enabled);

        // The malformed bundle is skipped, never surfaced.
        assert!(
            out.iter().all(|s| s.id != "broken"),
            "malformed SKILL.md is skipped"
        );

        // Deterministic order (by slug): my-skill < onboard.
        let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["my-skill", "onboard"]);
    }

    #[test]
    fn merge_with_no_source_dir_returns_only_deltas() {
        let deltas = vec![SkillState {
            slug: "web-research".to_string(),
            enabled: true,
            source: SkillSource::Registry,
            custom_doc: None,
        }];
        let out = merge_effective(None, deltas);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "web-research");
        assert_eq!(out[0].source, SkillSource::Registry);
    }
}
