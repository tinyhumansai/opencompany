//! `opencompany modules check` — the operator preflight (issue #1524).
//!
//! Answers "would the module load here?" without touching memory, the bus, or
//! `dlopen`: it resolves the artifact the way the load seam will, walks the
//! same ancestor ownership/mode rule tinybus enforces, and compares the
//! library's digest against the `modules.toml` allowlist shipped beside it.
//! Every verdict prints on its own line so a deploy log diff shows exactly
//! which gate moved.
//!
//! The ancestor rule is deliberately a **copy** of tinybus's
//! (`module/host.rs`: owned by self or root, and no group/other write bit
//! without the sticky bit) rather than a call into it: tinybus only exposes
//! the check as a refusal inside `dlopen`'s admission path, and a preflight
//! that loads the library to find out is not a preflight.

use std::fmt::Write as _;
use std::path::Path;

use sha2::Digest;

use super::ops::{MODULE_PATH_ENV, MODULE_STORE_SUBDIR};

/// The preflight report, one verdict per line, plus an overall pass.
pub struct Preflight {
    /// The rendered report.
    pub report: String,
    /// Whether every gate passed.
    pub ok: bool,
}

/// Runs every gate and renders the report.
#[must_use]
pub fn check() -> Preflight {
    fn verdict(report: &mut String, ok: &mut bool, line: &str, pass: bool) {
        let mark = if pass { "ok" } else { "FAIL" };
        let _ = writeln!(report, "{line}: {mark}");
        if !pass {
            *ok = false;
        }
    }

    let mut report = String::new();
    let mut ok = true;

    let _ = writeln!(
        report,
        "host_key: {}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let _ = writeln!(report, "store_subdir: {MODULE_STORE_SUBDIR}");

    let Some(path) = std::env::var(MODULE_PATH_ENV)
        .ok()
        .filter(|value| !value.is_empty())
    else {
        let _ = writeln!(report, "artifact ({MODULE_PATH_ENV} unset): FAIL");
        return Preflight { report, ok: false };
    };
    let path = std::path::PathBuf::from(path);
    let _ = writeln!(report, "artifact: {}", path.display());

    let exists = path.is_file();
    verdict(&mut report, &mut ok, "artifact present", exists);
    if !exists {
        return Preflight { report, ok: false };
    }

    let allowlist = path.parent().map(|dir| dir.join("modules.toml"));
    let allowlist_present = allowlist.as_deref().is_some_and(Path::is_file);
    // Present is the pass: tinybus treats an absent allowlist as opt-out and
    // loads UNVERIFIED, which a baked image must never ship.
    verdict(
        &mut report,
        &mut ok,
        "modules.toml beside the artifact",
        allowlist_present,
    );

    match digest_matches(&path, allowlist.as_deref()) {
        Some(true) => verdict(&mut report, &mut ok, "digest matches the allowlist", true),
        Some(false) => verdict(&mut report, &mut ok, "digest matches the allowlist", false),
        None => verdict(
            &mut report,
            &mut ok,
            "digest matches the allowlist (unreadable)",
            false,
        ),
    }

    let directories = directory_verdict(&path);
    verdict(
        &mut report,
        &mut ok,
        "ancestor ownership and modes",
        directories,
    );

    Preflight { report, ok }
}

/// Whether the library's sha256 appears in the allowlist beside it, keyed by
/// its file name (the same lookup tinybus performs at attach time).
fn digest_matches(library: &Path, allowlist: Option<&Path>) -> Option<bool> {
    let allowlist = allowlist?;
    let source = std::fs::read_to_string(allowlist).ok()?;
    let table: toml::Table = source.parse().ok()?;
    let name = library.file_name()?.to_str()?;
    let stem = library.file_stem()?.to_str()?;
    let expected = table
        .get(name)
        .or_else(|| table.get(stem))
        .and_then(toml::Value::as_str)?
        .to_ascii_lowercase();
    let bytes = std::fs::read(library).ok()?;
    let actual = format!("{:x}", sha2::Sha256::digest(&bytes));
    Some(actual == expected)
}

/// tinybus's ancestor rule, verbatim: every ancestor a directory, owned by
/// this uid or root, and no group/other write bit without the sticky bit.
#[cfg(unix)]
fn directory_verdict(library: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    unsafe extern "C" {
        fn getuid() -> u32;
    }
    let Some(start) = library.parent() else {
        return false;
    };
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        let Ok(cwd) = std::env::current_dir() else {
            return false;
        };
        cwd.join(start)
    };
    let uid = unsafe { getuid() };
    for component in absolute.ancestors() {
        let Ok(metadata) = std::fs::symlink_metadata(component) else {
            return false;
        };
        if !metadata.file_type().is_dir() {
            return false;
        }
        let owner = metadata.uid();
        let mode = metadata.mode();
        if owner != uid && owner != 0 {
            return false;
        }
        if mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return false;
        }
    }
    true
}

#[cfg(not(unix))]
fn directory_verdict(_library: &Path) -> bool {
    // The tenant image is linux; a non-unix preflight answers pass-with-note
    // rather than guessing at ACL semantics tinybus checks differently.
    true
}

#[cfg(test)]
mod tests {
    use super::digest_matches;
    use sha2::Digest;

    /// The digest gate matches tinybus's lookup: keyed by file name (or
    /// stem), compared against the library's actual sha256.
    #[test]
    fn the_digest_gate_compares_the_real_library_hash() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("libtinymemory_module.so");
        std::fs::write(&lib, b"not really a library").unwrap();
        let digest = format!("{:x}", sha2::Sha256::digest(b"not really a library"));
        let allowlist = dir.path().join("modules.toml");

        std::fs::write(
            &allowlist,
            format!("\"libtinymemory_module.so\" = \"{digest}\"\n"),
        )
        .unwrap();
        assert_eq!(digest_matches(&lib, Some(&allowlist)), Some(true));

        std::fs::write(&allowlist, "\"libtinymemory_module.so\" = \"deadbeef\"\n").unwrap();
        assert_eq!(digest_matches(&lib, Some(&allowlist)), Some(false));

        assert_eq!(
            digest_matches(&lib, Some(&dir.path().join("absent.toml"))),
            None
        );
    }
}
