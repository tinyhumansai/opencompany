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
//!
//! Every write here is gated
//! [`AdminScopedCompany`](crate::server::ops::AdminScopedCompany): a skill's
//! content becomes part of every agent's effective prompt, company-wide, so
//! installing, uninstalling, toggling, or authoring one decides something for
//! the company rather than for the caller alone. The two reads —
//! `GET …/skills` and `GET …/skills/registry` — stay open to any member; only
//! the writes decide anything.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::company::skill_effective::{self, EffectiveSkill};
use crate::company::skill_scan::{Verdict, scan_skill};
use crate::company::skill_validate::{MAX_SLUG_CHARS, validate_skill_md, validate_slug};
use crate::company::{SkillDoc, parse_skill_md, render_skill_md};
use crate::error::OpenCompanyError;
use crate::ports::skills_state::{SkillSource, SkillState};
use crate::ports::types::CompanyId;
use crate::server::error::ApiError;
use crate::server::ops::language;
use crate::server::ops::{AdminScopedCompany, ScopedCompany, scoped};

/// The default category stamped on a skill whose doc carries none.
const DEFAULT_CATEGORY: &str = "Ops";
/// The publisher stamped on shared-library skills (mirrors the GraphQL type).
const REGISTRY_PUBLISHER: &str = "OpenCompany";

/// The largest a skill's persisted `SKILL.md` (frontmatter and body together)
/// may be.
///
/// A skill's content lands in every agent's effective prompt, company-wide, so
/// this is a prompt budget rather than a storage limit. A quarter mebibyte
/// matches the codebase's existing ceiling for inline prose,
/// `MAX_ARTIFACT_BODY_BYTES` — generous for hand-authored instructions, and
/// still small enough that no single skill can quietly dominate what every
/// agent reads on every turn.
const MAX_SKILL_DOC_BYTES: usize = 256 * 1024;

/// Refuses a skill document over [`MAX_SKILL_DOC_BYTES`].
///
/// Checked on the assembled `SKILL.md` rather than the raw request fields, so
/// it bounds what actually lands in the agent's prompt regardless of which
/// field (name, description, or body) grew.
fn check_skill_doc_size(doc: &str) -> Result<(), ApiError> {
    if doc.len() > MAX_SKILL_DOC_BYTES {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "that skill is {:.1} KB — a skill's content has to be under {} KB.",
            doc.len() as f64 / 1024.0,
            MAX_SKILL_DOC_BYTES / 1024
        ))));
    }
    Ok(())
}

/// What the shared validator and the scan said about a document, as the
/// console renders it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanSummary {
    verdict: Verdict,
    /// One line per finding, in operator-facing language.
    findings: Vec<String>,
    /// Spec rules this document diverges from without being refused.
    spec_deltas: Vec<String>,
    /// Whether a blocking verdict was overridden for this one request.
    forced: bool,
}

/// Validates and scans an assembled `SKILL.md` before it can be stored.
///
/// Every entry point that accepts content an operator did not write calls this,
/// so registry install, the empty-registry fallback and console authoring
/// cannot disagree about what a skill is or what is wrong with one.
///
/// A blocking verdict refuses the write outright. `force` overrides that for
/// the one request and records that it did; there is deliberately no setting
/// that turns a class of finding off for a whole host, because a switch that
/// silences an alarm is the failure this scan exists to prevent.
fn vet_skill(slug: &str, doc: &str, force: bool) -> Result<ScanSummary, ApiError> {
    let valid = validate_skill_md(slug, doc)
        .map_err(|problems| ApiError(OpenCompanyError::InvalidRequest(problems.join(" "))))?;
    let report = scan_skill(&valid.doc, &[]);

    if report.is_blocked() && !force {
        return Err(ApiError(OpenCompanyError::InvalidRequest(format!(
            "that skill was refused by the content scan: {}. Review it, or resend with \
             `force: true` to install it anyway.",
            report.messages().join("; ")
        ))));
    }

    Ok(ScanSummary {
        verdict: report.verdict(),
        findings: report.messages(),
        spec_deltas: valid.deltas.iter().map(|delta| delta.message()).collect(),
        forced: force && report.is_blocked(),
    })
}

/// Per-company serialization for the skill write routes.
///
/// `install` and `create_custom` write a fresh [`SkillState`] straight through
/// [`SkillStateStore::set`](crate::ports::SkillStateStore::set) and are
/// raceless on their own — the store upserts by slug, so two of them landing
/// concurrently is an ordinary last-write-wins. `set_enabled` is the one
/// genuine read-modify-write: it lists the existing delta so it can preserve
/// the slug's `source` and `custom_doc`, then writes a new one back. An
/// install or an authoring landing in the middle of that window would be
/// silently reverted — its fresh doc and source overwritten by whatever
/// `set_enabled` read before it ran. Taking this lock unconditionally in every
/// write handler, exactly as `smtp.rs`'s `write_lock` does for its own
/// read-modify-write, keeps that ordering rule in one place rather than in
/// each handler.
fn write_lock(company: &CompanyId) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<CompanyId, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let locks = LOCKS.get_or_init(Default::default);
    let mut locks = locks.lock().expect("skill write locks poisoned");
    Arc::clone(
        locks
            .entry(company.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
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
    /// What the scan said, on the write that stored this skill. Absent on a
    /// read: the report belongs to the write that produced the document, and
    /// re-deriving one on every list would report a verdict nobody acted on.
    #[serde(skip_serializing_if = "Option::is_none")]
    scan: Option<ScanSummary>,
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
            scan: None,
        }
    }

    /// Attaches the report of the write that stored this skill.
    fn with_scan(mut self, scan: ScanSummary) -> Self {
        self.scan = Some(scan);
        self
    }

    /// Projects one entry of the company's effective set
    /// ([`skill_effective::resolve`]) to the console shape. An entry no layer
    /// supplied a document for is rendered from its slug alone.
    fn from_effective(skill: &EffectiveSkill) -> Self {
        let doc = skill.doc();
        Self {
            id: skill.slug.clone(),
            name: doc
                .map(|doc| doc.name.clone())
                .unwrap_or_else(|| titleize(&skill.slug)),
            description: doc.map(|doc| doc.description.clone()).unwrap_or_default(),
            category: doc
                .and_then(|doc| doc.category.clone())
                .unwrap_or_else(|| DEFAULT_CATEGORY.to_string()),
            source: skill.source,
            enabled: skill.enabled,
            version: doc.and_then(|doc| doc.version.clone()),
            scan: None,
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
    /// Install despite a blocking scan verdict, for this request only.
    #[serde(default)]
    force: bool,
}

/// The custom-skill body.
#[derive(Debug, Deserialize)]
struct CreateSkill {
    name: String,
    description: String,
    /// Save despite a blocking scan verdict, for this request only.
    #[serde(default)]
    force: bool,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// `GET …/skills` — the company's **effective** skill set, resolved by
/// [`skill_effective::resolve`]: the global baseline, the company's on-disk
/// bundles (`companies/<name>/skills/*/SKILL.md`), and the operator's
/// [`SkillStateStore`] deltas, with the manifest's `[globals].disable` folded in
/// as disabling deltas.
///
/// That is the same derivation the harness materializes for every agent, so the
/// console reports the set the agents actually have — a disabled skill included,
/// since its row is what carries the switch that turns it back on.
async fn list_skills(
    State(state): State<AppState>,
    company: ScopedCompany,
) -> Result<Json<Vec<InstalledSkill>>, ApiError> {
    let mut deltas = company.runtime.skills().list(company.id()).await?;
    deltas.extend(skill_effective::globals_skill_disables(
        &company.runtime.globals_disable().await?,
    ));
    let registry = state.shared_skill_registry()?;
    let effective = skill_effective::resolve(company.runtime.source_dir(), &registry, &deltas)?;
    Ok(Json(
        effective
            .iter()
            .map(InstalledSkill::from_effective)
            .collect(),
    ))
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
    company: AdminScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
    body: Option<Json<InstallSkill>>,
) -> Result<Json<InstalledSkill>, ApiError> {
    if let Err(problem) = validate_slug(&slug) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(problem)));
    }
    let meta = body.map(|Json(body)| body).unwrap_or_default();
    let force = meta.force;
    let lock = write_lock(company.id());
    let _guard = lock.lock().await;
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
            // instead of skipping a content-less delta. A client that supplies no
            // description gets the name as one: an empty scalar is a document the
            // parser refuses, and the delta it stored reached no agent.
            let name = meta
                .name
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| titleize(&slug));
            let description = meta
                .description
                .filter(|description| !description.trim().is_empty())
                .unwrap_or_else(|| name.clone());
            skill_md(&name, &description, meta.category.as_deref(), &description)
        }
    };
    check_skill_doc_size(&doc)?;
    let scan = vet_skill(&slug, &doc, force)?;
    let delta = SkillState {
        slug,
        enabled: true,
        source: SkillSource::Registry,
        custom_doc: Some(doc),
    };
    company.runtime.skills().set(company.id(), &delta).await?;
    Ok(Json(InstalledSkill::from_state(&delta).with_scan(scan)))
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
    company: AdminScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
) -> Result<StatusCode, ApiError> {
    let lock = write_lock(company.id());
    let _guard = lock.lock().await;
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
    company: AdminScopedCompany,
    Path(SlugPath { slug }): Path<SlugPath>,
    Json(body): Json<SetEnabled>,
) -> Result<Json<InstalledSkill>, ApiError> {
    if let Err(problem) = validate_slug(&slug) {
        return Err(ApiError(OpenCompanyError::InvalidRequest(problem)));
    }
    let lock = write_lock(company.id());
    let _guard = lock.lock().await;
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
    State(state): State<AppState>,
    company: AdminScopedCompany,
    Json(body): Json<CreateSkill>,
) -> Result<Json<InstalledSkill>, ApiError> {
    if body.name.trim().is_empty() || body.description.trim().is_empty() {
        return Err(ApiError(OpenCompanyError::InvalidRequest(
            language::SKILL_FIELDS_REQUIRED.to_string(),
        )));
    }
    let lock = write_lock(company.id());
    let _guard = lock.lock().await;
    let slug = unique_slug(
        &slugify(&body.name),
        &taken_slugs(&state, &company.runtime).await?,
    );
    let doc = skill_md(
        &body.name,
        &body.description,
        body.category.as_deref(),
        body.body.as_deref().unwrap_or(""),
    );
    check_skill_doc_size(&doc)?;
    let scan = vet_skill(&slug, &doc, body.force)?;
    let state = SkillState {
        slug,
        enabled: true,
        source: SkillSource::Custom,
        custom_doc: Some(doc),
    };
    company.runtime.skills().set(company.id(), &state).await?;
    Ok(Json(InstalledSkill::from_state(&state).with_scan(scan)))
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

/// Turns a display name into a filesystem-and-URL-safe slug, within
/// [`MAX_SLUG_CHARS`].
///
/// Authoring derives its store key and directory name from a free-text display
/// name, so whatever this returns has to be a slug the slug-bearing routes
/// accept. Truncating keeps a long name authorable; refusing it would leave the
/// operator renaming a skill to satisfy a limit they cannot see.
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
    let capped: String = slug.chars().take(MAX_SLUG_CHARS).collect();
    let trimmed = capped.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

/// Every slug the company already resolves — bundled, registry-installed and
/// authored alike.
///
/// Authoring has to avoid all three, not just the stored deltas: a bundled
/// skill has no delta row at all, so a check against the store alone would
/// still let an authored skill take `web-research` from the bundle.
async fn taken_slugs(
    state: &AppState,
    runtime: &crate::company::runtime::CompanyRuntime,
) -> Result<std::collections::HashSet<String>, ApiError> {
    let mut deltas = runtime.skills().list(runtime.id()).await?;
    deltas.extend(skill_effective::globals_skill_disables(
        &runtime.globals_disable().await?,
    ));
    let registry = state.shared_skill_registry()?;
    Ok(
        skill_effective::resolve(runtime.source_dir(), &registry, &deltas)?
            .into_iter()
            .map(|skill| skill.slug)
            .collect(),
    )
}

/// `base`, or the first free `base-2`, `base-3`, … within [`MAX_SLUG_CHARS`].
///
/// A slug is a store key and a directory name, and authoring derives it from a
/// free-text display name, so two names can arrive at one slug: they differ
/// only past the truncation point, or they contain no alphanumerics at all and
/// both fall back to `skill`. Writing under a taken slug replaces whatever
/// holds it — another authored skill, or a bundled document an agent reads —
/// so the collision is resolved here rather than at the store.
fn unique_slug(base: &str, taken: &std::collections::HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    for n in 2..=1000 {
        let suffix = format!("-{n}");
        let room = MAX_SLUG_CHARS.saturating_sub(suffix.chars().count());
        let stem = base.chars().take(room).collect::<String>();
        let stem = stem.trim_end_matches('-');
        let candidate = if stem.is_empty() {
            format!("skill{suffix}")
        } else {
            format!("{stem}{suffix}")
        };
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    format!("skill-{}", crate::ports::now_millis())
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
#[path = "skills_part2_tests.rs"]
mod tests_part2;
#[cfg(test)]
#[path = "skills_scan_tests.rs"]
mod tests_scan;
#[cfg(test)]
#[path = "skills_skill_md_frontmatter_resists_tests.rs"]
mod tests_skill_md_frontmatter_resists;
