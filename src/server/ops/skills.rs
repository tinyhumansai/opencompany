//! Skill writes: install/uninstall a registry skill, toggle enabled, and author
//! a custom skill — under both scope forms.
//!
//! Deltas land in the [`SkillStateStore`](crate::ports::SkillStateStore); the
//! built-in skill content stays on disk (seeded by
//! [`RuntimeBuilder::build`](crate::runtime::RuntimeBuilder)). The `InstalledSkill`
//! response mirrors the console's `@/api/skills` types: a custom skill's fields
//! come from its `SKILL.md`, and so do a registry install's — install snapshots
//! the shared library's document, so the delta is self-describing.
//!
//! The console holds no skill catalog of its own; it browses the shared library
//! over `GET …/skills/registry` and installs by slug, with the host resolving
//! the content.

use std::collections::HashMap;
use std::path::Path as FsPath;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::{SkillDoc, parse_skill_md, render_skill_md};
use crate::error::OpenCompanyError;
use crate::ports::skills_state::{SkillSource, SkillState};
use crate::server::error::ApiError;
use crate::server::ops::language;
use crate::server::ops::{ScopedCompany, scoped};

/// The default category stamped on a skill whose doc carries none.
const DEFAULT_CATEGORY: &str = "Ops";
/// The publisher stamped on shared-library skills (mirrors the GraphQL type).
const REGISTRY_PUBLISHER: &str = "OpenCompany";

/// Whether `slug` is a safe skill id: `^[a-z0-9][a-z0-9-]*$`. A slug is also a
/// directory name in the agent's scratch tree (`skills/<slug>/`), so a
/// traversal (`..`) or a path separator here would escape it. Mirrors
/// `harness::built_in::skills::valid_slug`.
fn valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Builds the skills route fragment.
pub fn router() -> Router<AppState> {
    scoped("/skills/{slug}/install", post(install))
        .merge(scoped("/skills/{slug}/uninstall", post(uninstall)))
        // `registry` is a static segment, so it wins over the `{slug}` pattern
        // above regardless of registration order (and the methods differ anyway).
        .merge(scoped("/skills/registry", get(list_registry)))
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
    /// The library revision this install snapshotted, when its doc carries one.
    /// Lets a future "update available" affordance diff an install against the
    /// live registry without any extra stored state.
    version: Option<String>,
}

impl InstalledSkill {
    /// Projects a [`SkillState`] to the console shape, parsing a custom skill's
    /// `SKILL.md` for its name/description/category and falling back to a
    /// slug-derived name for registry/built-in deltas.
    fn from_state(state: &SkillState) -> Self {
        let fallback = || {
            (
                titleize(&state.slug),
                String::new(),
                DEFAULT_CATEGORY.to_string(),
                None,
            )
        };
        let (name, description, category, version) = match &state.custom_doc {
            Some(doc) => match parse_skill_md(&state.slug, doc) {
                Ok(parsed) => (
                    parsed.name,
                    parsed.description,
                    parsed
                        .category
                        .unwrap_or_else(|| DEFAULT_CATEGORY.to_string()),
                    parsed.version,
                ),
                Err(_) => fallback(),
            },
            None => fallback(),
        };
        Self {
            id: state.slug.clone(),
            name,
            description,
            category,
            source: state.source,
            enabled: state.enabled,
            version,
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
            version: doc.version.clone(),
        }
    }
}

/// One skill in the shared library, as the console's registry tab browses it.
///
/// Deliberately **metadata only** — no `body`. Mirrors the GraphQL
/// `RegistrySkill` type so the two transports agree field for field.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistrySkill {
    id: String,
    name: String,
    description: String,
    category: String,
    publisher: String,
    /// The library revision this entry ships, from frontmatter. `None` for a
    /// skill authored before `version` existed.
    version: Option<String>,
}

impl RegistrySkill {
    fn from_doc(doc: &SkillDoc) -> Self {
        Self {
            id: doc.slug.clone(),
            name: doc.name.clone(),
            description: doc.description.clone(),
            category: doc
                .category
                .clone()
                .unwrap_or_else(|| DEFAULT_CATEGORY.to_string()),
            publisher: REGISTRY_PUBLISHER.to_string(),
            version: doc.version.clone(),
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
                    existing.version = doc.version;
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

/// `POST …/skills/{slug}/install` — install a shared-library skill by slug.
///
/// **Server-authoritative.** The persisted `SKILL.md` is the shared library's own
/// document — frontmatter *and* body verbatim, so the agent gets the whole
/// procedure. The request body is ignored whenever the library can serve the
/// slug: a client cannot dictate what a registry skill contains.
///
/// Resolution, in order:
///
/// 1. **Slug in the registry** → persist that document. The snapshot is pinned:
///    a later library edit does not rewrite an existing install.
/// 2. **Slug absent from a non-empty registry** → `404`. This is a typo or a
///    stale client; silently persisting a stub is what produced content-less
///    installs in the first place.
/// 3. **Empty registry** → fall back to the client's metadata, as before. An
///    empty registry means this host serves no shared library at all
///    (platform-provisioned mode, no `skills_root`), so there is nothing to
///    resolve against and refusing every install would break hosted tenants
///    outright.
///
/// A *configured* library that fails to load is a `500`, never case 3: silently
/// degrading a broken shared library to "no library" would hand the client
/// authorship of a registry skill's contents on exactly the hosts that meant to
/// be server-authoritative.
async fn install(
    State(state): State<AppState>,
    company: ScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
    body: Option<Json<InstallSkill>>,
) -> Result<Json<InstalledSkill>, ApiError> {
    if !valid_slug(&slug) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "`{slug}` is not a valid skill slug. Skills live under `skills/<slug>/`, so a slug \
             is `[a-z0-9][a-z0-9-]*`."
        ))));
    }
    let registry = state.shared_skill_registry()?;
    let doc = match registry.iter().find(|doc| doc.slug == slug) {
        Some(doc) => render_skill_md(doc),
        None if !registry.is_empty() => {
            return Err(ApiError(OpenCompanyError::NotFound(
                language::SKILL_NOT_IN_REGISTRY.to_string(),
            )));
        }
        None => {
            // No shared library backs this host. Persist a real `SKILL.md` built
            // from the client's metadata (the description doubles as the body) so
            // `EffectiveSkills::materialize` surfaces the skill to the agent
            // instead of skipping a content-less delta.
            let meta = body.map(|Json(b)| b).unwrap_or_default();
            let name = meta
                .name
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| titleize(&slug));
            let description = meta.description.unwrap_or_default();
            skill_md(&name, &description, meta.category.as_deref(), &description)
        }
    };
    let delta = SkillState {
        slug,
        enabled: true,
        source: SkillSource::Registry,
        custom_doc: Some(doc),
    };
    company.runtime.skills().set(company.id(), &delta).await?;
    Ok(Json(InstalledSkill::from_state(&delta)))
}

/// `GET …/skills/registry` — the shared skill library the console's registry tab
/// browses.
///
/// **Metadata only, by construction**: [`RegistrySkill`] has no `body` field, so
/// the payload stays flat regardless of how large the library grows. Install is
/// server-authoritative, so the client never needs a body — it posts a slug and
/// the host resolves the content.
///
/// Scoped (and so authorized) like every other console route even though the
/// library itself is host-global; the registry is not public.
async fn list_registry(
    State(state): State<AppState>,
    _company: ScopedCompany,
) -> Result<Json<Vec<RegistrySkill>>, ApiError> {
    Ok(Json(
        state
            .shared_skill_registry()?
            .iter()
            .map(RegistrySkill::from_doc)
            .collect(),
    ))
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
    if !valid_slug(&slug) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "`{slug}` is not a valid skill slug. Skills live under `skills/<slug>/`, so a slug \
             is `[a-z0-9][a-z0-9-]*`."
        ))));
    }
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

    /// `valid_slug` is the gate both write handlers share: a slug is also a
    /// directory name under `skills/<slug>/`, so a traversal (`..`) or a path
    /// separator (`/`) must never reach the filesystem, and the alphabet is
    /// lowercase-only. The path extractor can only ever hand a handler a single
    /// segment, so `a/b` cannot arrive as a path — but the function is the
    /// contract every slug-bearing caller routes through, so it is the right
    /// place to pin all three shapes the review named.
    #[test]
    fn valid_slug_rejects_traversal_separator_and_case() {
        // The shapes the review named.
        assert!(!valid_slug(".."), "parent traversal");
        assert!(!valid_slug("a/b"), "path separator");
        assert!(!valid_slug("A"), "uppercase start");
        // And the rest of the boundary.
        assert!(!valid_slug(""), "empty");
        assert!(!valid_slug("-leading"), "leading dash");
        assert!(!valid_slug("has space"), "interior space");
        assert!(
            !valid_slug("under_score"),
            "underscore is not in the alphabet"
        );
        assert!(!valid_slug("UPPER"), "all uppercase");
        // And the shape that must pass.
        assert!(valid_slug("a-1"), "lowercase, digit, dash");
        assert!(valid_slug("0"), "single digit");
        assert!(valid_slug("seo-audit"), "typical slug");
    }

    /// HTTP-level coverage of the two path-slug handlers. A slug that fails
    /// `valid_slug` must be rejected with `400` **before** any write, so the
    /// effective skill set is untouched; a valid slug succeeds and lands.
    ///
    /// `..` and `a/b` cannot be carried as a single path segment (a `/` splits
    /// them, and `..` is normalized away by the router), so their rejection is
    /// pinned in [`valid_slug_rejects_traversal_separator_and_case`]. `A` is a
    /// single segment the router will pass through, so it is the shape we drive
    /// through the handlers to prove the `400` and the no-mutation guarantee.
    mod http {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use serde_json::Value;
        use tower::ServiceExt;

        use crate::company::CompanyManifest;
        use crate::ports::CompanyStore;
        use crate::ports::types::{CompanyId, CompanyRecord};
        use crate::runtime::RuntimeBuilder;
        use crate::server::router;
        use crate::server::test_support::{fixed_cookie, seed_fixed_admin};
        use crate::{AppConfig, AppState};

        async fn state_with_company(home: &std::path::Path) -> AppState {
            let id = CompanyId::new("acme");
            let manifest: CompanyManifest =
                toml::from_str("[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n").unwrap();
            crate::store::FsCompanyStore::new(home.to_path_buf())
                .save(&CompanyRecord {
                    overlay_retired_agents: Vec::new(),
                    overlay_agent_edits: Vec::new(),
                    id: id.clone(),
                    manifest: manifest.clone(),
                    ledger: Vec::new(),
                    lifecycle: "running".to_string(),
                    overlay_agents: Vec::new(),
                    overlay_desk_members: Vec::new(),
                    overlay_desk_order: Vec::new(),
                    overlay_desks: Vec::new(),
                    overlay_workflows: Vec::new(),
                    overlay_budgets: Vec::new(),
                    overlay_policy: None,
                    overlay_tool_grants: None,
                    overlay_desk_tools: Default::default(),
                    disabled_workflows: Vec::new(),
                    template_provenance: None,
                    setup: None,
                    name_confirmed: false,
                    activation_completed_at: None,
                    created_at_millis: None,
                })
                .await
                .unwrap();
            let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
                .with_id(id.clone())
                .build()
                .await
                .unwrap();
            let state = AppState::new(AppConfig::default());
            state.registry().insert(id, std::sync::Arc::new(runtime));
            seed_fixed_admin(&state, "acme").await;
            state
        }

        async fn send(
            state: &AppState,
            method: &str,
            uri: &str,
            body: Option<&str>,
        ) -> (StatusCode, Value, String) {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header("cookie", fixed_cookie("acme"));
            let request = match body {
                Some(body) => request
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
                None => request.body(Body::empty()).unwrap(),
            };
            let response = router(state.clone()).oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let raw = String::from_utf8_lossy(&bytes).to_string();
            let value = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap_or(Value::Null)
            };
            (status, value, raw)
        }

        /// The effective skill set, as the console reads it.
        async fn slugs(state: &AppState) -> Vec<String> {
            let (status, value, raw) = send(state, "GET", "/api/v1/company/skills", None).await;
            assert_eq!(status, StatusCode::OK, "list skills: {raw}");
            value
                .as_array()
                .expect("skills list is an array")
                .iter()
                .map(|s| s["id"].as_str().expect("an id").to_string())
                .collect()
        }

        /// Both write handlers reject an invalid slug with `400` and leave the
        /// effective skill set untouched; a valid slug then succeeds and lands.
        #[tokio::test]
        async fn invalid_slugs_are_400_and_leave_state_unchanged() {
            let home = tempfile::tempdir().unwrap();
            let state = state_with_company(home.path()).await;

            let before = slugs(&state).await;

            // `install` rejects the uppercase slug without writing.
            let (status, _, raw) =
                send(&state, "POST", "/api/v1/company/skills/A/install", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "install A: {raw}");
            assert!(
                raw.contains("not a valid skill slug"),
                "the 400 explains why: {raw}"
            );

            // `set_enabled` rejects the same slug without writing.
            let (status, _, raw) = send(
                &state,
                "PUT",
                "/api/v1/company/skills/A",
                Some(r#"{"enabled":true}"#),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "set_enabled A: {raw}");

            // Neither attempt mutated the effective set.
            assert_eq!(
                slugs(&state).await,
                before,
                "a rejected slug must not land a delta"
            );

            // A valid slug succeeds on both handlers and does land.
            let (status, _, raw) =
                send(&state, "POST", "/api/v1/company/skills/a-1/install", None).await;
            assert_eq!(status, StatusCode::OK, "install a-1: {raw}");

            let (status, _, raw) = send(
                &state,
                "PUT",
                "/api/v1/company/skills/a-1",
                Some(r#"{"enabled":false}"#),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "set_enabled a-1: {raw}");

            assert!(
                slugs(&state).await.iter().any(|s| s == "a-1"),
                "the valid slug lands in the effective set"
            );
        }
    }
}
