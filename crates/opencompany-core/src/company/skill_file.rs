//! SKILL.md documents: `skills/<slug>/SKILL.md` (repo-level and per-company).
//!
//! A skill is a Markdown file with a small `---`-fenced frontmatter block
//! carrying `name`, `description`, and an optional `category`. The frontmatter
//! is hand-parsed — no serde_yaml dependency — and the Markdown body is
//! preserved verbatim so WS4 can feed it to OpenHuman's skill parser unchanged.

use std::path::{Path, PathBuf};

use crate::error::{OpenCompanyError, Result};

/// A parsed SKILL.md document.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillDoc {
    /// The skill's directory name (its slug).
    pub slug: String,
    /// Display name, from frontmatter.
    pub name: String,
    /// One-line description, from frontmatter.
    pub description: String,
    /// Optional grouping category, from frontmatter.
    pub category: Option<String>,
    /// Optional publisher version, from frontmatter (e.g. `1.0.0`).
    ///
    /// Purely descriptive: nothing compares or orders it yet. It rides inside a
    /// registry install's snapshotted `SKILL.md`, so an installed skill records
    /// which revision it pinned and a later "update available" affordance can
    /// diff the installed snapshot against the live registry.
    pub version: Option<String>,
    /// The Markdown body after the frontmatter, preserved verbatim.
    pub body: String,
    /// Frontmatter lines this parser does not recognise, verbatim.
    ///
    /// The parser ignores an unknown key, but an uploaded document is stored
    /// and materialized as its own source, so whatever those lines say is what
    /// the agent reads. Keeping them means the scan can see the whole of what
    /// will be stored rather than only the keys this struct names.
    pub extra_frontmatter: Vec<String>,
}

/// Parses one SKILL.md document for the given `slug` (its directory name).
///
/// The frontmatter must be a `---`-fenced block of `key: value` lines at the
/// very top; `name` and `description` are required, `category` is optional, and
/// any other keys are tolerated. The body after the closing fence is kept
/// verbatim.
pub fn parse_skill_md(slug: &str, src: &str) -> Result<SkillDoc> {
    let path = PathBuf::from(format!("{slug}/SKILL.md"));

    let (frontmatter, body) =
        split_frontmatter(src).ok_or_else(|| OpenCompanyError::DataParse {
            path: path.clone(),
            message: "missing a `---` frontmatter block at the top of the file.".to_string(),
        })?;

    let mut extra_frontmatter = Vec::new();
    let mut name = None;
    let mut description = None;
    let mut category = None;
    let mut version = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            extra_frontmatter.push(line.to_string());
            continue;
        };
        let value = value.trim().to_string();
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "category" => category = Some(value),
            "version" => version = Some(value),
            _ => extra_frontmatter.push(line.to_string()),
        }
    }

    let mut problems = Vec::new();
    let name = match name {
        Some(name) if !name.is_empty() => name,
        _ => {
            problems.push(format!(
                "skill `{slug}` is missing a `name` in its frontmatter."
            ));
            String::new()
        }
    };
    let description = match description {
        Some(description) if !description.is_empty() => description,
        _ => {
            problems.push(format!(
                "skill `{slug}` is missing a `description` in its frontmatter."
            ));
            String::new()
        }
    };
    if !problems.is_empty() {
        return Err(OpenCompanyError::DataInvalid { path, problems });
    }

    Ok(SkillDoc {
        slug: slug.to_string(),
        name,
        description,
        category: category.filter(|value| !value.is_empty()),
        version: version.filter(|value| !value.is_empty()),
        body: body.to_string(),
        extra_frontmatter,
    })
}

/// Renders a [`SkillDoc`] back to `SKILL.md` source: a `---`-fenced frontmatter
/// block followed by the body verbatim.
///
/// This is the inverse of [`parse_skill_md`] for everything the parser keeps —
/// `parse → render → parse` is a fixed point on the doc (the round-trip test
/// below pins that). It is **not** byte-identical to the original source: the
/// parser trims each scalar and drops unknown frontmatter keys, so a rendered
/// doc is the canonical form rather than a faithful copy. Registry installs
/// snapshot the original source directly, so nothing round-trips through here
/// on the hot path — it exists so a doc assembled in memory can be persisted.
///
/// Each scalar is collapsed to one line (newlines become spaces), matching the
/// line-based parser: that stops a value from injecting extra frontmatter keys
/// or emitting a bare `---` that would close the block early.
pub fn render_skill_md(doc: &SkillDoc) -> String {
    let one_line = |s: &str| s.replace(['\n', '\r'], " ").trim().to_string();
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", one_line(&doc.name)));
    out.push_str(&format!("description: {}\n", one_line(&doc.description)));
    if let Some(category) = &doc.category {
        out.push_str(&format!("category: {}\n", one_line(category)));
    }
    if let Some(version) = &doc.version {
        out.push_str(&format!("version: {}\n", one_line(version)));
    }
    out.push_str("---\n");
    out.push_str(&doc.body);
    out
}

/// The bundle directory the skill registry lists first.
///
/// The global baseline (`companies/_globals`) is not a company, but its
/// `skills/` are what `[skills].always` installs everywhere, so they head the
/// registry and win any slug a vertical also ships.
const BASELINE_BUNDLE: &str = "_globals";

/// Loads the skill registry: every `<bundle>/skills/<slug>/SKILL.md` under a
/// `companies/` directory, one document per slug, sorted by slug.
///
/// There is no separate shared library — a skill lives in the bundle it
/// belongs to, and the registry is the union. When two bundles ship the same
/// slug the first in registry order keeps it: the baseline, then every other
/// bundle in name order. Each company still materializes its *own* bundle's
/// copy (`harness::built_in::skills`); this only decides what the registry
/// offers under that slug.
///
/// A missing `companies_dir` yields an empty list, exactly like
/// [`load_dir_skills`]; a bundle without a `skills/` directory contributes
/// nothing. A malformed `SKILL.md` anywhere fails the whole load, because a
/// registry that silently dropped a document would serve a different catalog
/// from the one on disk.
pub fn load_catalog_skills(companies_dir: &Path) -> Result<Vec<SkillDoc>> {
    if !companies_dir.exists() {
        return Ok(Vec::new());
    }
    let entries =
        std::fs::read_dir(companies_dir).map_err(|source| OpenCompanyError::DataRead {
            path: companies_dir.to_path_buf(),
            source,
        })?;
    let mut bundles = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| OpenCompanyError::DataRead {
            path: companies_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
        {
            bundles.push((name.to_string(), path));
        }
    }
    // Baseline first, then name order — the precedence the doc above promises.
    bundles.sort_by(|a, b| {
        (a.0 != BASELINE_BUNDLE)
            .cmp(&(b.0 != BASELINE_BUNDLE))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut by_slug: std::collections::BTreeMap<String, SkillDoc> =
        std::collections::BTreeMap::new();
    for (_, bundle) in bundles {
        for doc in load_dir_skills(&bundle.join("skills"))? {
            by_slug.entry(doc.slug.clone()).or_insert(doc);
        }
    }
    Ok(by_slug.into_values().collect())
}

/// Loads every `<slug>/SKILL.md` under a directory, sorted by slug.
///
/// A missing directory yields an empty list; a subdirectory without a
/// `SKILL.md` is skipped. [`load_catalog_skills`] composes this per bundle.
pub fn load_dir_skills(dir: &Path) -> Result<Vec<SkillDoc>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut slugs = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|source| OpenCompanyError::DataRead {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| OpenCompanyError::DataRead {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("SKILL.md").is_file()
            && let Some(slug) = path.file_name().and_then(|name| name.to_str())
        {
            slugs.push(slug.to_string());
        }
    }
    slugs.sort();

    let mut out = Vec::with_capacity(slugs.len());
    for slug in slugs {
        let file = dir.join(&slug).join("SKILL.md");
        let text = std::fs::read_to_string(&file).map_err(|source| OpenCompanyError::DataRead {
            path: file.clone(),
            source,
        })?;
        // Re-label parse/validation errors with the real on-disk path.
        let doc = match parse_skill_md(&slug, &text) {
            Ok(doc) => doc,
            Err(OpenCompanyError::DataInvalid { problems, .. }) => {
                return Err(OpenCompanyError::DataInvalid {
                    path: file,
                    problems,
                });
            }
            Err(OpenCompanyError::DataParse { message, .. }) => {
                return Err(OpenCompanyError::DataParse {
                    path: file,
                    message,
                });
            }
            Err(other) => return Err(other),
        };
        out.push(doc);
    }
    Ok(out)
}

/// Splits a document into its frontmatter inner text and its verbatim body.
///
/// Returns `None` when the document does not open with a `---` fence line or
/// has no matching closing fence.
pub(super) fn split_frontmatter(src: &str) -> Option<(&str, &str)> {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let after_open = strip_fence_line(src)?;

    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let frontmatter = &after_open[..offset];
            let body = &after_open[offset + line.len()..];
            return Some((frontmatter, body));
        }
        offset += line.len();
    }
    None
}

/// Consumes a leading `---` fence line, returning the text after it. The rest of
/// that line must be blank.
fn strip_fence_line(src: &str) -> Option<&str> {
    let rest = src.strip_prefix("---")?;
    match rest.find('\n') {
        Some(newline) if rest[..newline].trim().is_empty() => Some(&rest[newline + 1..]),
        Some(_) => None,
        None if rest.trim().is_empty() => Some(""),
        None => None,
    }
}

#[cfg(test)]
#[path = "skill_file_tests.rs"]
mod tests;
