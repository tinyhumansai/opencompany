//! Filesystem bundle layout for a company's durable state.
//!
//! Every company owns a directory tree under an OpenCompany home root:
//!
//! ```text
//! <home>/companies/<slug>/
//!   company.toml      # the materialized manifest (charter + roster)
//!   meta.json         # lifecycle state and other bundle metadata
//!   events.jsonl      # append-only event log
//!   ledger.jsonl      # append-only ledger
//!   memory/           # compressed traces + task results
//!   context/          # content-addressed context blobs + index
//!   secrets/          # per-company secret files (0700 on unix)
//!   keys/             # Ed25519 identity seed (0700 dir, 0600 files)
//! ```
//!
//! `secrets/` and `keys/` are excluded from bundle exports (see
//! [`Bundle::EXPORT_EXCLUDES`]) so a shared bundle never leaks the company's
//! signing key or per-company secrets.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;

/// Resolves the OpenCompany home root.
///
/// Honours `OPENCOMPANY_HOME`, otherwise falls back to `~/.opencompany`
/// (computed from `$HOME`). No `dirs` dependency.
pub fn default_home() -> PathBuf {
    if let Ok(home) = std::env::var("OPENCOMPANY_HOME") {
        return PathBuf::from(home);
    }
    let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join(".opencompany")
}

/// Converts a company id into a filesystem-safe directory name.
///
/// Company ids are typically already slugs, but this defends against ids that
/// contain path separators or other unsafe characters.
fn slug(id: &CompanyId) -> String {
    let raw = id.as_ref();
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// The on-disk directory layout for one company.
#[derive(Clone, Debug)]
pub struct Bundle {
    dir: PathBuf,
}

impl Bundle {
    /// Resolves the bundle directory for `id` under `root`.
    pub fn new(root: impl Into<PathBuf>, id: &CompanyId) -> Self {
        let dir = root.into().join("companies").join(slug(id));
        Self { dir }
    }

    /// The company's bundle directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path to the materialized manifest.
    pub fn company_toml(&self) -> PathBuf {
        self.dir.join("company.toml")
    }

    /// Path to the bundle metadata (lifecycle state).
    pub fn meta_json(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    /// Path to the append-only event log.
    pub fn events_jsonl(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    /// Path to the append-only ledger.
    pub fn ledger_jsonl(&self) -> PathBuf {
        self.dir.join("ledger.jsonl")
    }

    /// Path to the runtime journal (at-most-once effects + approval queue).
    pub fn journal_jsonl(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    /// The memory subdirectory (traces + task results).
    pub fn memory_dir(&self) -> PathBuf {
        self.dir.join("memory")
    }

    /// Path to the compressed-trace log.
    pub fn traces_jsonl(&self) -> PathBuf {
        self.memory_dir().join("traces.jsonl")
    }

    /// Path to the task-result log.
    pub fn tasks_jsonl(&self) -> PathBuf {
        self.memory_dir().join("tasks.jsonl")
    }

    /// The context subdirectory.
    pub fn context_dir(&self) -> PathBuf {
        self.dir.join("context")
    }

    /// The content-addressed blob subdirectory.
    pub fn context_blobs_dir(&self) -> PathBuf {
        self.context_dir().join("blobs")
    }

    /// Path to a single context blob by address.
    pub fn context_blob(&self, addr: &str) -> PathBuf {
        self.context_blobs_dir().join(addr)
    }

    /// Path to the context index.
    pub fn context_index_jsonl(&self) -> PathBuf {
        self.context_dir().join("index.jsonl")
    }

    /// The per-company feedback subdirectory (the "feedback family").
    pub fn feedback_dir(&self) -> PathBuf {
        self.dir.join("feedback")
    }

    /// Path to the append-only feedback-item log.
    pub fn feedback_items_jsonl(&self) -> PathBuf {
        self.feedback_dir().join("items.jsonl")
    }

    /// The per-company secrets subdirectory.
    pub fn secrets_dir(&self) -> PathBuf {
        self.dir.join("secrets")
    }

    /// Path to a single secret file by key.
    pub fn secret(&self, key: &str) -> PathBuf {
        self.secrets_dir().join(slug(&CompanyId::new(key)))
    }

    /// The per-company key material subdirectory (`0700` on unix).
    pub fn keys_dir(&self) -> PathBuf {
        self.dir.join("keys")
    }

    /// Path to the Ed25519 identity seed (`0600` on unix).
    pub fn agent_key(&self) -> PathBuf {
        self.keys_dir().join("agent.ed25519")
    }

    /// Bundle subdirectories excluded from exports. A shared or copied bundle
    /// must never carry the company's private key or per-company secrets; an
    /// export flow honours this list unless explicitly overridden.
    pub const EXPORT_EXCLUDES: &'static [&'static str] = &["secrets", "keys"];

    /// Returns the bundle subdirectories a copy/export must skip.
    pub fn export_excludes() -> &'static [&'static str] {
        Self::EXPORT_EXCLUDES
    }

    /// Creates every directory in the bundle layout if absent.
    pub async fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.dir.clone(),
            self.memory_dir(),
            self.context_blobs_dir(),
            self.secrets_dir(),
            self.keys_dir(),
        ] {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|source| OpenCompanyError::StoreIo {
                    path: dir.clone(),
                    source,
                })?;
        }
        restrict_dir(&self.secrets_dir())?;
        restrict_dir(&self.keys_dir())?;
        Ok(())
    }
}

/// Restricts a directory to owner-only access (`0700`) on unix.
///
/// A no-op on non-unix targets. Secret encryption-at-rest is a documented
/// follow-up; Phase 1 relies on filesystem permissions and per-company path
/// isolation.
#[cfg(unix)]
fn restrict_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(dir, perms).map_err(|source| OpenCompanyError::StoreIo {
        path: dir.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Restricts a file to owner read/write only (`0600`) on unix.
///
/// Used for identity key material (`keys/agent.ed25519`). A no-op on non-unix
/// targets, which rely on directory isolation instead. Gated to the sole
/// consumer (the `tinyplace` signer) so the default build has no dead code.
#[cfg(all(unix, feature = "tinyplace"))]
pub(crate) fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(|source| OpenCompanyError::StoreIo {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(all(not(unix), feature = "tinyplace"))]
pub(crate) fn restrict_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn slug_sanitizes_unsafe_characters() {
        assert_eq!(slug(&CompanyId::new("acme-co")), "acme-co");
        assert_eq!(slug(&CompanyId::new("a/b/../c")), "a_b_.._c");
        assert_eq!(slug(&CompanyId::new("")), "_");
    }

    #[test]
    fn bundle_paths_nest_under_company_slug() {
        let bundle = Bundle::new("/root", &CompanyId::new("acme"));
        assert!(bundle.dir().ends_with("companies/acme"));
        assert!(
            bundle
                .events_jsonl()
                .ends_with("companies/acme/events.jsonl")
        );
        assert!(bundle.traces_jsonl().ends_with("memory/traces.jsonl"));
        assert!(
            bundle
                .context_index_jsonl()
                .ends_with("context/index.jsonl")
        );
    }

    #[test]
    fn keys_paths_nest_and_are_excluded_from_exports() {
        let bundle = Bundle::new("/root", &CompanyId::new("acme"));
        assert!(bundle.keys_dir().ends_with("companies/acme/keys"));
        assert!(bundle.agent_key().ends_with("keys/agent.ed25519"));
        assert!(Bundle::export_excludes().contains(&"keys"));
        assert!(Bundle::export_excludes().contains(&"secrets"));
    }

    #[test]
    fn default_home_prefers_env_override() {
        // SAFETY: single-threaded test; restores prior state.
        let prev = std::env::var("OPENCOMPANY_HOME").ok();
        unsafe { std::env::set_var("OPENCOMPANY_HOME", "/custom/home") };
        assert_eq!(default_home(), PathBuf::from("/custom/home"));
        match prev {
            Some(v) => unsafe { std::env::set_var("OPENCOMPANY_HOME", v) },
            None => unsafe { std::env::remove_var("OPENCOMPANY_HOME") },
        }
    }
}
