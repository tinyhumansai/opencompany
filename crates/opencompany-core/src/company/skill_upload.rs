//! Reading an uploaded skill: a bare `SKILL.md`, or an archive that carries
//! one.
//!
//! This module answers one question — what document did the operator upload,
//! and under which slug — and refuses everything it cannot answer that for. It
//! deliberately stops there: [`skill_validate`](super::skill_validate) decides
//! whether the document is acceptable and [`skill_scan`](super::skill_scan)
//! decides whether its text is safe, so an upload passes the same two gates a
//! registry install and a console-authored skill do.
//!
//! ## The archive is the attack surface
//!
//! A `.zip` is a list of paths and byte counts supplied by whoever built it,
//! and every one of those is hostile input. The shape checks here run over the
//! archive's directory **before** a single entry is decompressed, so a bomb is
//! refused by arithmetic rather than by running out of memory:
//!
//! * an entry count ceiling ([`MAX_ARCHIVE_ENTRIES`]);
//! * the sum of the declared uncompressed sizes against
//!   [`MAX_ARCHIVE_BYTES`] — a 10 MB entry inside a 40 KB archive is visible
//!   here and nowhere later;
//! * absolute paths, `..` traversal, and backslash-separated paths;
//! * symlinks, which are how an archive reaches a path it never names;
//! * an archive nested inside the archive.
//!
//! The declared sizes are the archive's own claim, so the one entry that is
//! read is read through a bounded reader as well, behind whatever the archive
//! reader itself does with a header that disagrees with its entry.
//!
//! ## Bundled resource files are refused, not dropped
//!
//! `SkillState.custom_doc` is a single document, so there is nowhere to put a
//! script or a reference file an archive carries. An archive with extras is
//! therefore refused with its file names in the message. Storing the `SKILL.md`
//! and silently discarding the rest would hand the operator a skill whose
//! procedure references files no agent will ever find.

use std::io::Read;

use super::skill_validate::{MAX_SLUG_CHARS, slugify, validate_slug};

/// The most entries an uploaded archive may declare.
///
/// A skill is one document, and — until bundled resources have somewhere to
/// live — an archive holding one is a `SKILL.md` and the directories above it.
/// The ceiling is far above that and still low enough that the shape pass is
/// bounded work on an archive the host has not yet trusted.
pub const MAX_ARCHIVE_ENTRIES: usize = 64;

/// The most an uploaded archive may hold once expanded, in bytes.
///
/// Checked against the sum of the entries' **declared** uncompressed sizes
/// before anything is decompressed, which is the only point at which a zip bomb
/// is cheap to refuse. Four times the single-document ceiling, so a legitimate
/// archive carrying the largest `SKILL.md` the write plane will store has
/// headroom for the directory entries around it.
pub const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024;

/// The file name an archive's skill document has to have.
pub const SKILL_DOC_NAME: &str = "SKILL.md";

/// A document read off an upload, ready for validation and the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadedSkill {
    /// The slug the document will be stored under — the archive's top
    /// directory when it has one, otherwise derived from the frontmatter name.
    pub slug: String,
    /// The `SKILL.md` source, verbatim.
    pub doc: String,
}

/// Reads one uploaded file into the document it carries.
///
/// `filename` picks the reader: `.md` is the document itself, `.zip` and
/// `.skill` are archives containing one. Anything else is refused by extension
/// rather than by sniffing, so an operator is told which formats the route
/// takes instead of watching a `.tar.gz` fail as a malformed archive.
///
/// The refusal is a plain sentence because it is rendered against the file's
/// own row in the upload dialog: several files are uploaded at once and each
/// gets its own outcome, so this cannot be a whole-request error.
pub fn read_upload(filename: &str, bytes: &[u8]) -> Result<UploadedSkill, String> {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".md") {
        return read_markdown(bytes);
    }
    if lower.ends_with(".zip") || lower.ends_with(".skill") {
        return read_archive(bytes);
    }
    Err("only `.md`, `.zip` and `.skill` files can be uploaded as skills.".to_string())
}

/// Reads a bare `SKILL.md`, whose slug comes from its own frontmatter name.
///
/// There is no directory to take a slug from, so the document names itself the
/// same way console authoring does — through [`slugify`] over the display name.
fn read_markdown(bytes: &[u8]) -> Result<UploadedSkill, String> {
    let doc = decode(bytes)?;
    let name = frontmatter_name(&doc).ok_or(
        "that file has no `name` in its frontmatter, so there is nothing to store it under. A \
         skill starts with a `---` block carrying `name` and `description`.",
    )?;
    Ok(UploadedSkill {
        slug: slugify(&name),
        doc,
    })
}

/// Reads an archive that carries exactly one `SKILL.md` and nothing else.
/// Whether an archive entry is macOS bookkeeping rather than skill content.
///
/// Right-clicking a folder and choosing Compress is how an operator on a Mac
/// makes a skill archive, and Finder puts an `__MACOSX/` tree of AppleDouble
/// sidecars beside the folder plus a `.DS_Store` inside it. Counting those
/// makes the archive read as two top-level directories carrying bundled
/// extras, so the upload is refused for a shape the operator cannot see and
/// did not choose. They are dropped after the path and symlink checks, which
/// still apply to every entry.
fn is_mac_metadata(path: &str) -> bool {
    let mut segments = path.split('/');
    if segments.clone().any(|segment| segment == "__MACOSX") {
        return true;
    }
    segments
        .next_back()
        .is_some_and(|name| name == ".DS_Store" || name.starts_with("._"))
}

fn read_archive(bytes: &[u8]) -> Result<UploadedSkill, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|error| format!("that archive could not be read: {error}."))?;

    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(format!(
            "that archive holds {} entries — an uploaded skill may hold {MAX_ARCHIVE_ENTRIES}.",
            archive.len()
        ));
    }

    let mut declared: u64 = 0;
    let mut files = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| format!("that archive could not be read: {error}."))?;
        let path = entry.name().to_string();
        check_entry_path(&path)?;
        if is_symlink(entry.unix_mode()) {
            return Err(format!(
                "`{path}` in that archive is a symbolic link. An uploaded skill is read as files, \
                 and a link is how an archive reaches a path it never names."
            ));
        }
        declared = declared.saturating_add(entry.size());
        if declared > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "that archive expands to more than {} KB, which is more than an uploaded skill \
                 may hold.",
                MAX_ARCHIVE_BYTES / 1024
            ));
        }
        if entry.is_dir() {
            continue;
        }
        if is_nested_archive(&path) {
            return Err(format!(
                "`{path}` in that archive is itself an archive. An uploaded skill is read one \
                 level deep."
            ));
        }
        if is_mac_metadata(&path) {
            continue;
        }
        files.push(path);
    }

    let root = single_root(&files)?;
    let wanted = match &root {
        Some(dir) => format!("{dir}/{SKILL_DOC_NAME}"),
        None => SKILL_DOC_NAME.to_string(),
    };
    if !files.iter().any(|path| path == &wanted) {
        return Err(format!(
            "that archive has no `{SKILL_DOC_NAME}`. A skill archive carries one at the top \
             level, or inside a single directory."
        ));
    }

    let extras: Vec<&str> = files
        .iter()
        .filter(|path| *path != &wanted)
        .map(String::as_str)
        .collect();
    if !extras.is_empty() {
        return Err(format!(
            "that archive also carries {}. A skill stores one document, so there is nowhere to \
             keep bundled files — upload a `{SKILL_DOC_NAME}` on its own rather than have them \
             dropped.",
            join_names(&extras)
        ));
    }

    let doc = {
        let entry = archive
            .by_name(&wanted)
            .map_err(|error| format!("`{wanted}` could not be read: {error}."))?;
        // The sizes above are the archive's own claim about itself. Reading
        // through a bound turns a header that lies about its entry into a
        // refusal instead of whatever the entry actually expands to.
        let mut buffer = Vec::new();
        entry
            .take(MAX_ARCHIVE_BYTES + 1)
            .read_to_end(&mut buffer)
            .map_err(|error| format!("`{wanted}` could not be read: {error}."))?;
        if buffer.len() as u64 > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "`{wanted}` expands to more than {} KB, which is more than an uploaded skill may \
                 hold.",
                MAX_ARCHIVE_BYTES / 1024
            ));
        }
        decode(&buffer)?
    };

    let slug = match root {
        Some(dir) => {
            validate_slug(&dir).map_err(|problem| {
                format!("that archive's directory is not a usable skill name. {problem}")
            })?;
            dir
        }
        None => slugify(&frontmatter_name(&doc).ok_or_else(|| {
            format!(
                "that archive's `{SKILL_DOC_NAME}` has no `name` in its frontmatter, so there is \
                 nothing to store it under."
            )
        })?),
    };

    Ok(UploadedSkill { slug, doc })
}

/// Refuses an archive entry whose path escapes the archive.
///
/// Every form is refused by name rather than by normalizing the path, so the
/// message says which one was found and a test can aim at one shape each.
fn check_entry_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("that archive holds an entry with no name.".to_string());
    }
    if path.contains('\\') {
        return Err(format!(
            "`{path}` in that archive is not a relative path. Archive entries separate \
             directories with `/`."
        ));
    }
    if path.starts_with('/') || is_windows_absolute(path) {
        return Err(format!(
            "`{path}` in that archive is an absolute path. An uploaded skill is read as a \
             directory of its own, so every entry has to sit inside it."
        ));
    }
    if path.split('/').any(|part| part == "..") {
        return Err(format!(
            "`{path}` in that archive points outside it. An uploaded skill is read as a directory \
             of its own, so every entry has to sit inside it."
        ));
    }
    if path.chars().count() > MAX_SLUG_CHARS * 8 {
        return Err("that archive holds an entry with an unreasonably long path.".to_string());
    }
    Ok(())
}

/// Whether `path` is a Windows drive-qualified path (`C:\…`, `C:/…`).
fn is_windows_absolute(path: &str) -> bool {
    let mut chars = path.chars();
    matches!((chars.next(), chars.next()), (Some(c), Some(':')) if c.is_ascii_alphabetic())
}

/// Whether the entry's Unix mode marks it a symbolic link.
fn is_symlink(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & 0o170_000 == 0o120_000)
}

/// Whether the entry's own name says it is another archive.
fn is_nested_archive(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        ".zip", ".skill", ".tar", ".gz", ".tgz", ".bz2", ".xz", ".7z", ".rar",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}

/// The single top directory every file sits under, or `None` when they all sit
/// at the archive's root.
///
/// Anything else — two top directories, or a file at the root beside a
/// directory — is refused, because the spec's shape is a skill directory and
/// there is no rule that would pick between two of them.
fn single_root(files: &[String]) -> Result<Option<String>, String> {
    let mut roots = Vec::new();
    let mut at_root = false;
    for path in files {
        match path.split_once('/') {
            Some((head, _)) => {
                if !roots.iter().any(|seen| seen == head) {
                    roots.push(head.to_string());
                }
            }
            None => at_root = true,
        }
    }
    match (at_root, roots.len()) {
        (_, 0) => Ok(None),
        (false, 1) => Ok(Some(roots.remove(0))),
        _ => Err(format!(
            "that archive has no single `{SKILL_DOC_NAME}`. A skill archive carries one at the \
             top level, or inside a single directory."
        )),
    }
}

/// Decodes upload bytes as UTF-8.
///
/// Refused rather than replaced: a document that is not text is not a
/// `SKILL.md`, and lossy decoding would store replacement characters in every
/// agent's prompt.
fn decode(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| "that file is not UTF-8 text, so it is not a `SKILL.md`.".to_string())
}

/// The `name` scalar from a document's frontmatter, when it has one.
fn frontmatter_name(doc: &str) -> Option<String> {
    let (frontmatter, _) = super::skill_file::split_frontmatter(doc)?;
    frontmatter.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim().eq_ignore_ascii_case("name") && !value.trim().is_empty())
            .then(|| value.trim().to_string())
    })
}

/// Renders a handful of file names as a readable list.
fn join_names(names: &[&str]) -> String {
    const SHOWN: usize = 3;
    let shown: Vec<String> = names
        .iter()
        .take(SHOWN)
        .map(|name| format!("`{name}`"))
        .collect();
    match names.len().checked_sub(SHOWN) {
        Some(rest) if rest > 0 => format!("{} and {rest} more", shown.join(", ")),
        _ => shown.join(", "),
    }
}

#[cfg(test)]
#[path = "skill_upload_tests.rs"]
mod tests;
