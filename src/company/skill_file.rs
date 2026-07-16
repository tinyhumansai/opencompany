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
    /// The Markdown body after the frontmatter, preserved verbatim.
    pub body: String,
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

    let mut name = None;
    let mut description = None;
    let mut category = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "category" => category = Some(value),
            _ => {}
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
        body: body.to_string(),
    })
}

/// Loads every `<slug>/SKILL.md` under a directory, sorted by slug.
///
/// Doubles as the repo-level shared skill registry loader. A missing directory
/// yields an empty list; a subdirectory without a `SKILL.md` is skipped.
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
fn split_frontmatter(src: &str) -> Option<(&str, &str)> {
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
mod tests {
    use super::*;

    const WEB_RESEARCH: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/skills/web-research/SKILL.md"
    ));
    const WEEKLY_REPORT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/skills/weekly-report/SKILL.md"
    ));

    #[test]
    fn parses_both_shipped_repo_skills() {
        let web = parse_skill_md("web-research", WEB_RESEARCH).expect("web-research is valid");
        assert_eq!(web.slug, "web-research");
        assert_eq!(web.name, "Web Research");
        assert!(web.description.starts_with("Answer a question"));
        assert_eq!(web.category, None);
        // Body is preserved verbatim, including its heading.
        assert!(web.body.contains("# Web Research"));
        assert!(web.body.contains("## When to use"));

        let weekly =
            parse_skill_md("weekly-report", WEEKLY_REPORT).expect("weekly-report is valid");
        assert_eq!(weekly.name, "Weekly Report");
    }

    #[test]
    fn reads_optional_category_and_tolerates_unknown_keys() {
        let src = "---\nname: Demo\ndescription: A demo skill\ncategory: research\nowner: eve\n---\n# Demo\n";
        let doc = parse_skill_md("demo", src).expect("valid");
        assert_eq!(doc.category.as_deref(), Some("research"));
        assert_eq!(doc.body, "# Demo\n");
    }

    #[test]
    fn missing_frontmatter_is_a_parse_error() {
        let err = parse_skill_md("demo", "# No frontmatter here\n").unwrap_err();
        assert_eq!(err.code(), "data_parse");
    }

    #[test]
    fn unterminated_frontmatter_is_a_parse_error() {
        let err = parse_skill_md("demo", "---\nname: Demo\n").unwrap_err();
        assert_eq!(err.code(), "data_parse");
    }

    #[test]
    fn missing_required_keys_is_a_validation_error() {
        let err = parse_skill_md("demo", "---\ncategory: research\n---\nbody\n").unwrap_err();
        assert_eq!(err.code(), "data_invalid");
        let message = err.to_string();
        assert!(message.contains("`name`"), "{message}");
        assert!(message.contains("`description`"), "{message}");
    }

    #[test]
    fn body_is_preserved_verbatim_including_trailing_content() {
        let src = "---\nname: N\ndescription: D\n---\n\n# Heading\n\nBody with [[a link]].\n";
        let doc = parse_skill_md("demo", src).expect("valid");
        assert_eq!(doc.body, "\n# Heading\n\nBody with [[a link]].\n");
    }
}
