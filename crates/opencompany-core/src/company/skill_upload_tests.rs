//! What an upload is allowed to be.
//!
//! Every archive here is crafted rather than described. A cap asserted against
//! a hand-written struct proves the arithmetic; only an archive that actually
//! carries the traversal, the symlink or the bomb proves the reader refuses one
//! — and the reader is the only thing standing between a stranger's `.zip` and
//! the host's filesystem.

use super::*;

use std::io::{Cursor, Write};

use zip::write::{SimpleFileOptions, ZipWriter};

const DOC: &str = "---\nname: Press Outreach\ndescription: Pitch a story.\n---\nSteps.\n";

/// Builds an archive from `(path, contents)` pairs, stored uncompressed so the
/// declared sizes in the directory are the real ones.
fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (path, contents) in entries {
        if path.ends_with('/') {
            writer.add_directory(*path, options).unwrap();
            continue;
        }
        writer.start_file(*path, options).unwrap();
        writer.write_all(contents).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// Builds an archive whose one entry is a symbolic link.
fn symlink_archive(path: &str, target: &str) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_symlink(path, target, SimpleFileOptions::default())
        .unwrap();
    writer.finish().unwrap().into_inner()
}

#[test]
fn a_markdown_upload_is_stored_under_a_slug_from_its_own_name() {
    let read = read_upload("press-outreach.md", DOC.as_bytes()).unwrap();
    assert_eq!(read.slug, "press-outreach");
    assert_eq!(read.doc, DOC);
}

#[test]
fn a_markdown_upload_without_a_frontmatter_name_is_refused() {
    let problem = read_upload("skill.md", b"---\ndescription: No name.\n---\nBody.\n").unwrap_err();
    assert!(problem.contains("`name`"), "{problem}");
}

#[test]
fn a_file_that_is_not_markdown_or_an_archive_is_refused_by_extension() {
    let problem = read_upload("skill.tar.gz", b"whatever").unwrap_err();
    assert!(problem.contains("`.md`"), "{problem}");
}

#[test]
fn an_archive_with_the_document_at_its_root_takes_its_slug_from_the_name() {
    let read = read_upload("skill.zip", &archive(&[("SKILL.md", DOC.as_bytes())])).unwrap();
    assert_eq!(read.slug, "press-outreach");
    assert_eq!(read.doc, DOC);
}

#[test]
fn an_archive_with_one_top_directory_takes_its_slug_from_that_directory() {
    let bytes = archive(&[
        ("press-outreach/", b""),
        ("press-outreach/SKILL.md", DOC.as_bytes()),
    ]);
    let read = read_upload("bundle.skill", &bytes).unwrap();
    assert_eq!(read.slug, "press-outreach");
}

#[test]
fn an_archive_whose_directory_is_not_a_usable_slug_is_refused() {
    let bytes = archive(&[("Press Outreach/SKILL.md", DOC.as_bytes())]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("not a usable skill name"), "{problem}");
}

#[test]
fn an_archive_with_two_top_directories_is_refused() {
    let bytes = archive(&[
        ("one/SKILL.md", DOC.as_bytes()),
        ("two/SKILL.md", DOC.as_bytes()),
    ]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("single directory"), "{problem}");
}

#[test]
fn an_archive_without_a_skill_document_is_refused() {
    let bytes = archive(&[("press-outreach/README.md", b"Not a skill.")]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("SKILL.md"), "{problem}");
}

/// The 6.3 decision: bundled files have nowhere to live yet, so an archive that
/// carries them is refused **by name**. The failure this avoids is the silent
/// one — storing the document and discarding the script it tells the agent to
/// run.
#[test]
fn an_archive_carrying_bundled_files_is_refused_rather_than_having_them_dropped() {
    let bytes = archive(&[
        ("press-outreach/SKILL.md", DOC.as_bytes()),
        ("press-outreach/pitch.py", b"print('hi')"),
        ("press-outreach/refs/contacts.csv", b"a,b\n"),
    ]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("pitch.py"), "{problem}");
    assert!(problem.contains("contacts.csv"), "{problem}");
    assert!(
        problem.contains("nowhere to keep bundled files"),
        "{problem}"
    );
}

#[test]
fn an_archive_entry_that_climbs_out_of_the_archive_is_refused() {
    let bytes = archive(&[("../escaped/SKILL.md", DOC.as_bytes())]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("points outside it"), "{problem}");
}

#[test]
fn an_archive_entry_that_climbs_out_mid_path_is_refused() {
    let bytes = archive(&[("press-outreach/../../etc/SKILL.md", DOC.as_bytes())]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("points outside it"), "{problem}");
}

#[test]
fn an_archive_entry_with_an_absolute_path_is_refused() {
    let bytes = archive(&[("/etc/cron.d/SKILL.md", DOC.as_bytes())]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("absolute path"), "{problem}");
}

#[test]
fn an_archive_entry_with_a_windows_drive_path_is_refused() {
    let bytes = archive(&[("C:/windows/SKILL.md", DOC.as_bytes())]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("absolute path"), "{problem}");
}

#[test]
fn an_archive_entry_separated_by_backslashes_is_refused() {
    let bytes = archive(&[("press-outreach\\SKILL.md", DOC.as_bytes())]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("not a relative path"), "{problem}");
}

#[test]
fn an_archive_holding_a_symbolic_link_is_refused() {
    let bytes = symlink_archive("press-outreach/SKILL.md", "/etc/passwd");
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("symbolic link"), "{problem}");
}

#[test]
fn an_archive_holding_another_archive_is_refused() {
    let bytes = archive(&[
        ("press-outreach/SKILL.md", DOC.as_bytes()),
        ("press-outreach/more.zip", b"PK\x03\x04"),
    ]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("itself an archive"), "{problem}");
}

#[test]
fn an_archive_with_too_many_entries_is_refused_before_any_are_read() {
    let names: Vec<String> = (0..=MAX_ARCHIVE_ENTRIES)
        .map(|i| format!("press-outreach/file{i}.txt"))
        .collect();
    let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), b"x" as &[u8])).collect();
    let problem = read_upload("skill.zip", &archive(&entries)).unwrap_err();
    assert!(problem.contains("entries"), "{problem}");
}

/// A bomb declares its expansion in the directory, so the sum of the declared
/// sizes refuses it without a byte being decompressed.
#[test]
fn an_archive_that_declares_more_than_the_cap_is_refused_before_extracting() {
    let big = vec![b'a'; (MAX_ARCHIVE_BYTES as usize) + 1];
    let bytes = archive(&[
        ("press-outreach/SKILL.md", DOC.as_bytes()),
        ("press-outreach/big.txt", &big),
    ]);
    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("expands to more than"), "{problem}");
}

/// A directory that understates an entry's real length cannot smuggle the
/// difference past the size pass: the archive reader stops at the declared
/// length and the entry fails its own checksum, and the bounded read in
/// `read_archive` stands behind that. Either way the upload is refused, which
/// is the property worth pinning — a crafted header must not produce a stored
/// skill.
#[test]
fn an_archive_whose_header_understates_its_document_is_refused() {
    let real = (MAX_ARCHIVE_BYTES as usize) + 4096;
    let mut doc = String::from("---\nname: Big\ndescription: Big.\n---\n");
    doc.push_str(&"a".repeat(real - doc.len()));
    let mut bytes = archive(&[("SKILL.md", doc.as_bytes())]);

    // Stored entries record the uncompressed length verbatim as a
    // little-endian u32 in both the local header and the central directory.
    // Rewriting both is exactly the lie a hand-built archive tells.
    let truth = (real as u32).to_le_bytes();
    let lie = 16u32.to_le_bytes();
    let mut patched = 0;
    for start in 0..bytes.len().saturating_sub(4) {
        if bytes[start..start + 4] == truth {
            bytes[start..start + 4].copy_from_slice(&lie);
            patched += 1;
        }
    }
    assert!(
        patched >= 2,
        "the crafted archive must understate the entry in both headers, patched {patched}"
    );

    let problem = read_upload("skill.zip", &bytes).unwrap_err();
    assert!(problem.contains("could not be read"), "{problem}");
}

#[test]
fn a_document_that_is_not_utf8_is_refused() {
    let problem = read_upload("skill.md", &[0xff, 0xfe, 0x00]).unwrap_err();
    assert!(problem.contains("UTF-8"), "{problem}");
}

#[test]
fn a_file_that_is_not_an_archive_at_all_is_refused_as_unreadable() {
    let problem = read_upload("skill.zip", b"not a zip").unwrap_err();
    assert!(problem.contains("could not be read"), "{problem}");
}
