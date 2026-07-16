//! Skill writes: install/uninstall a registry skill, toggle enabled, and author
//! a custom skill — under both scope forms.
//!
//! Deltas land in the [`SkillStateStore`](crate::ports::SkillStateStore); the
//! built-in skill content stays on disk (seeded by
//! [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder)). The `InstalledSkill`
//! response mirrors the console (`frontend/src/lib/skills.ts`); for registry /
//! built-in skills the name/description are best-effort (the console enriches
//! from its catalog), while a custom skill's fields come from its `SKILL.md`.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::parse_skill_md;
use crate::error::OpenCompanyError;
use crate::ports::skills_state::{SkillSource, SkillState};
use crate::server::error::ApiError;
use crate::server::ops::language;
use crate::server::ops::{ScopedCompany, scoped};

/// Builds the skills route fragment.
pub fn router() -> Router<AppState> {
    scoped("/skills/{slug}/install", post(install))
        .merge(scoped("/skills/{slug}/uninstall", post(uninstall)))
        .merge(scoped("/skills/{slug}", put(set_enabled)))
        .merge(scoped("/skills", post(create_custom)))
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
                    parsed.category.unwrap_or_else(|| "Ops".to_string()),
                ),
                Err(_) => (titleize(&state.slug), String::new(), "Ops".to_string()),
            },
            None => (titleize(&state.slug), String::new(), "Ops".to_string()),
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

async fn install(
    company: ScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
) -> Result<Json<InstalledSkill>, ApiError> {
    let state = SkillState {
        slug,
        enabled: true,
        source: SkillSource::Registry,
        custom_doc: None,
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
    let doc = build_skill_md(&body);
    let state = SkillState {
        slug,
        enabled: true,
        source: SkillSource::Custom,
        custom_doc: Some(doc),
    };
    company.runtime.skills().set(company.id(), &state).await?;
    Ok(Json(InstalledSkill::from_state(&state)))
}

/// Builds a `SKILL.md` document from the custom-skill fields.
fn build_skill_md(body: &CreateSkill) -> String {
    let category = body
        .category
        .as_deref()
        .map(|c| format!("category: {c}\n"))
        .unwrap_or_default();
    let content = body.body.as_deref().unwrap_or("");
    format!(
        "---\nname: {}\ndescription: {}\n{category}---\n{content}\n",
        body.name, body.description
    )
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
