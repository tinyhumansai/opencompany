//! Skill reads: `Company.skills` (company-dir docs unioned with the operator's
//! [`SkillStateStore`] deltas) and the top-level `skillRegistry` (the repo-level
//! shared library).
//!
//! The store holds deltas only; the effective set unions the company's on-disk
//! `skills/*/SKILL.md` docs at read time — matching the write-plane semantics
//! documented on [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder).

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::{Context, ID, SimpleObject};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::company::{SkillDoc, load_dir_skills, parse_skill_md};
use crate::ports::skills_state::{SkillSource, SkillState};
use crate::store::Bundle;

/// One skill installed in a company. Mirrors `frontend/src/lib/skills.ts`.
#[derive(SimpleObject)]
#[graphql(name = "Skill")]
pub struct SkillGql {
    /// The skill slug.
    pub id: ID,
    /// The display name.
    pub name: String,
    /// A one-line description.
    pub description: String,
    /// The skill category (e.g. `Marketing`, `Ops`).
    pub category: String,
    /// Provenance: `company` | `registry` | `custom`.
    pub source: String,
    /// Whether the skill is enabled for the company.
    pub enabled: bool,
}

/// One skill in the shared repo-level registry, installable into any company.
#[derive(SimpleObject)]
#[graphql(name = "RegistrySkill")]
pub struct RegistrySkillGql {
    /// The skill slug.
    pub id: ID,
    /// The display name.
    pub name: String,
    /// A one-line description.
    pub description: String,
    /// The skill category.
    pub category: String,
    /// The publisher of the registry skill.
    pub publisher: String,
}

/// The default category when a skill doc carries none.
const DEFAULT_CATEGORY: &str = "Ops";
/// The publisher stamped on repo-level registry skills.
const REGISTRY_PUBLISHER: &str = "OpenCompany";

fn source_str(source: SkillSource) -> &'static str {
    match source {
        SkillSource::Company => "company",
        SkillSource::Registry => "registry",
        SkillSource::Custom => "custom",
    }
}

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

fn from_doc(doc: &SkillDoc, source: &str, enabled: bool) -> SkillGql {
    SkillGql {
        id: ID(doc.slug.clone()),
        name: doc.name.clone(),
        description: doc.description.clone(),
        category: doc
            .category
            .clone()
            .unwrap_or_else(|| DEFAULT_CATEGORY.to_string()),
        source: source.to_string(),
        enabled,
    }
}

/// The repo-level skill registry docs, loaded from `{home}/skills`.
fn registry_docs(state: &AppState) -> Arc<[SkillDoc]> {
    let dir = state.home().join("skills");
    state.skill_registry(&dir).unwrap_or_else(|_| Arc::from([]))
}

/// Resolves `Company.skills`: company-dir docs overlaid with store deltas.
pub(crate) async fn resolve_company(
    ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<Vec<SkillGql>> {
    let state = ctx.data::<AppState>()?;
    let registry = registry_docs(state);

    // Base: the company's own on-disk skills, all enabled by default.
    let company_dir = Bundle::new(state.home(), runtime.id()).dir().join("skills");
    let mut by_slug: HashMap<String, SkillGql> = load_dir_skills(&company_dir)
        .unwrap_or_default()
        .iter()
        .map(|doc| (doc.slug.clone(), from_doc(doc, "company", true)))
        .collect();

    // Overlay the operator's deltas (enabled toggles, installs, custom skills).
    for st in runtime.skills().list(runtime.id()).await? {
        if let Some(existing) = by_slug.get_mut(&st.slug) {
            existing.enabled = st.enabled;
            existing.source = source_str(st.source).to_string();
        } else {
            by_slug.insert(st.slug.clone(), skill_from_state(&st, &registry));
        }
    }

    let mut out: Vec<SkillGql> = by_slug.into_values().collect();
    out.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    Ok(out)
}

/// Projects a store delta with no company-dir doc into a `Skill`, enriching a
/// registry install from the shared library and a custom skill from its own
/// `SKILL.md`.
fn skill_from_state(st: &SkillState, registry: &[SkillDoc]) -> SkillGql {
    match st.source {
        SkillSource::Registry => match registry.iter().find(|doc| doc.slug == st.slug) {
            Some(doc) => from_doc(doc, "registry", st.enabled),
            None => SkillGql {
                id: ID(st.slug.clone()),
                name: titleize(&st.slug),
                description: String::new(),
                category: DEFAULT_CATEGORY.to_string(),
                source: "registry".to_string(),
                enabled: st.enabled,
            },
        },
        SkillSource::Custom => {
            let doc = st
                .custom_doc
                .as_deref()
                .and_then(|src| parse_skill_md(&st.slug, src).ok());
            match doc {
                Some(doc) => from_doc(&doc, "custom", st.enabled),
                None => SkillGql {
                    id: ID(st.slug.clone()),
                    name: titleize(&st.slug),
                    description: String::new(),
                    category: DEFAULT_CATEGORY.to_string(),
                    source: "custom".to_string(),
                    enabled: st.enabled,
                },
            }
        }
        SkillSource::Company => SkillGql {
            id: ID(st.slug.clone()),
            name: titleize(&st.slug),
            description: String::new(),
            category: DEFAULT_CATEGORY.to_string(),
            source: "company".to_string(),
            enabled: st.enabled,
        },
    }
}

/// Resolves the top-level `skillRegistry`.
pub(crate) async fn resolve_registry(
    ctx: &Context<'_>,
) -> async_graphql::Result<Vec<RegistrySkillGql>> {
    let state = ctx.data::<AppState>()?;
    Ok(registry_docs(state)
        .iter()
        .map(|doc| RegistrySkillGql {
            id: ID(doc.slug.clone()),
            name: doc.name.clone(),
            description: doc.description.clone(),
            category: doc
                .category
                .clone()
                .unwrap_or_else(|| DEFAULT_CATEGORY.to_string()),
            publisher: REGISTRY_PUBLISHER.to_string(),
        })
        .collect())
}
