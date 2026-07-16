//! Workspace seeding: `companies/<name>/workspace/**`.
//!
//! A company's template workspace is an Obsidian-style tree of folders and
//! Markdown notes linked with `[[wiki links]]`. [`walk_workspace`] flattens it
//! into a deterministic seed tree (folders + Markdown only), and
//! [`extract_wikilinks`] pulls a note's outbound links — shared by seeding
//! (WS3) and the backlinks resolver (WS2).

use std::path::{Path, PathBuf};

use crate::error::{OpenCompanyError, Result};

/// Whether a seed node is a folder or a Markdown note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A directory.
    Folder,
    /// A `.md` note.
    Markdown,
}

/// One node in a workspace seed tree.
#[derive(Clone, Debug, PartialEq)]
pub struct SeedNode {
    /// Path relative to the workspace root.
    pub rel_path: PathBuf,
    /// Whether this node is a folder or a Markdown note.
    pub kind: NodeKind,
    /// The note's Markdown content; `None` for folders.
    pub content: Option<String>,
}

/// Walks a workspace directory into a deterministic, sorted seed tree.
///
/// Only folders and `.md` files are included; every other file is skipped, as
/// are symlinks (so the walk can never escape the root). A missing directory
/// yields an empty list. The result is sorted by `rel_path` so seeding is
/// reproducible.
pub fn walk_workspace(dir: &Path) -> Result<Vec<SeedNode>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    walk_dir(dir, dir, &mut out)?;
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

fn walk_dir(root: &Path, current: &Path, out: &mut Vec<SeedNode>) -> Result<()> {
    let mut entries = Vec::new();
    let read = std::fs::read_dir(current).map_err(|source| OpenCompanyError::DataRead {
        path: current.to_path_buf(),
        source,
    })?;
    for entry in read {
        let entry = entry.map_err(|source| OpenCompanyError::DataRead {
            path: current.to_path_buf(),
            source,
        })?;
        entries.push(entry.path());
    }
    entries.sort();

    for path in entries {
        // Never follow symlinks — they are the only way a walk could escape the
        // root — and reject any relative path that climbs out of it.
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|source| OpenCompanyError::DataRead {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            continue;
        }

        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        reject_escaping(root, &rel)?;

        if metadata.is_dir() {
            out.push(SeedNode {
                rel_path: rel,
                kind: NodeKind::Folder,
                content: None,
            });
            walk_dir(root, &path, out)?;
        } else if is_markdown(&path) {
            let content =
                std::fs::read_to_string(&path).map_err(|source| OpenCompanyError::DataRead {
                    path: path.clone(),
                    source,
                })?;
            out.push(SeedNode {
                rel_path: rel,
                kind: NodeKind::Markdown,
                content: Some(content),
            });
        }
        // Non-Markdown files are skipped.
    }
    Ok(())
}

/// True when `path` has a `.md` extension (case-insensitive).
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Rejects a relative path that is absolute or climbs above the root.
fn reject_escaping(root: &Path, rel: &Path) -> Result<()> {
    use std::path::Component;
    let escapes = rel.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if escapes {
        return Err(OpenCompanyError::DataInvalid {
            path: root.to_path_buf(),
            problems: vec![format!(
                "workspace note `{}` escapes the workspace root — notes must stay inside `workspace/`.",
                rel.display()
            )],
        });
    }
    Ok(())
}

/// Extracts the targets of `[[wiki links]]` from Markdown, in order.
///
/// Handles both `[[target]]` and `[[target|alias]]` (the target before the
/// pipe is returned, trimmed). This is a pure function so seeding and the
/// backlinks resolver share one definition.
pub fn extract_wikilinks(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = md;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let inner = &after[..end];
        let target = inner.split('|').next().unwrap_or("").trim();
        if !target.is_empty() {
            out.push(target.to_string());
        }
        rest = &after[end + 2..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_and_aliased_wikilinks_in_order() {
        let md = "See [[Spring launch]] and [[Brand voice|how we sound]], plus [[ Campaign checklist ]].";
        let links = extract_wikilinks(md);
        assert_eq!(
            links,
            vec!["Spring launch", "Brand voice", "Campaign checklist"]
        );
    }

    #[test]
    fn ignores_empty_and_unterminated_links() {
        assert_eq!(
            extract_wikilinks("[[]] and [[unterminated"),
            Vec::<String>::new()
        );
        assert_eq!(
            extract_wikilinks("a [[|only alias]] b"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn walk_is_deterministic_and_markdown_only() {
        let dir = std::env::temp_dir().join(format!("oc-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Brand")).unwrap();
        std::fs::create_dir_all(dir.join("Campaigns")).unwrap();
        std::fs::write(dir.join("README.md"), "# Root\n[[Brand voice]]").unwrap();
        std::fs::write(dir.join("Brand/Brand voice.md"), "# Voice").unwrap();
        std::fs::write(dir.join("Campaigns/notes.txt"), "ignored").unwrap();
        std::fs::write(dir.join("cover.png"), b"\x89PNG").unwrap();

        let nodes = walk_workspace(&dir).unwrap();
        let paths: Vec<String> = nodes
            .iter()
            .map(|n| n.rel_path.display().to_string())
            .collect();
        // Sorted, folders + markdown only; the .txt and .png are skipped.
        assert_eq!(
            paths,
            vec!["Brand", "Brand/Brand voice.md", "Campaigns", "README.md"]
        );

        let readme = nodes
            .iter()
            .find(|n| n.rel_path == Path::new("README.md"))
            .unwrap();
        assert_eq!(readme.kind, NodeKind::Markdown);
        assert_eq!(readme.content.as_deref(), Some("# Root\n[[Brand voice]]"));

        let brand = nodes
            .iter()
            .find(|n| n.rel_path == Path::new("Brand"))
            .unwrap();
        assert_eq!(brand.kind, NodeKind::Folder);
        assert_eq!(brand.content, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_directory_walks_to_empty() {
        let dir = std::env::temp_dir().join("oc-ws-does-not-exist-xyz");
        assert!(walk_workspace(&dir).unwrap().is_empty());
    }

    #[test]
    fn path_traversal_is_rejected() {
        let root = Path::new("/tmp/workspace");
        let err = reject_escaping(root, Path::new("../secrets.md")).unwrap_err();
        assert_eq!(err.code(), "data_invalid");
        assert!(err.to_string().contains("escapes the workspace root"));
        // A normal nested path is accepted.
        assert!(reject_escaping(root, Path::new("Brand/Brand voice.md")).is_ok());
    }
}
