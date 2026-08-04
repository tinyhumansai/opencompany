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
//!
//! [`resolve_home`] resolves the `<home>` root every bundle hangs off, and is
//! the only place that decision is made.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;

/// The one environment knob that places an instance's data root. Also read by
/// [`data_dir_from_env`](crate::app::config::data_dir_from_env) for the
/// workspace layout, so a single value moves an entire instance.
pub const DATA_DIR_ENV: &str = "OPENCOMPANY_DATA_DIR";

/// A knob that never worked. It was documented in this module and read by a
/// helper no binary path ever called, so exporting it silently did nothing.
/// [`resolve_home`] now rejects it by name instead of ignoring it.
const REMOVED_HOME_ENV: &str = "OPENCOMPANY_HOME";

/// The legacy leaf appended to `$HOME/.opencompany` when neither `--home` nor
/// [`DATA_DIR_ENV`] is set. [`Bundle::new`] appends `companies/` of its own, so
/// the untouched default resolves bundles to
/// `~/.opencompany/companies/companies/<slug>`. That extra level is a wart, but
/// it is where every existing local install's data already sits, so the default
/// is preserved verbatim rather than silently relocating it.
const LEGACY_DEFAULT_LEAF: &str = "companies";

/// Resolves the OpenCompany home — the root every [`Bundle`] hangs off — from
/// the `--home` flag and the process environment.
///
/// Precedence, highest first:
///
/// 1. **`--home`** (`flag`). Outranks [`DATA_DIR_ENV`] and the legacy default
///    below. It does not suppress the [`REMOVED_HOME_ENV`] rejection, which is
///    checked ahead of every branch — see [Errors](#errors) — so `--home` wins
///    the *choice* of root but never skips that validation.
/// 2. **`OPENCOMPANY_DATA_DIR`** ([`DATA_DIR_ENV`]), used verbatim. This is the
///    same value a hosted tenant's entrypoint already forwards as
///    `--home "$OPENCOMPANY_DATA_DIR"`, so the flag and the variable resolve
///    identically and the workspace layout and the company bundles share one
///    root (`<root>/companies/<slug>`, matching
///    [`DataLayout::companies_dir`](crate::store::DataLayout::companies_dir)).
/// 3. **`$HOME/.opencompany/companies`**, the legacy local default — see
///    [`LEGACY_DEFAULT_LEAF`]. Falls back to a relative `.opencompany/companies`
///    when `$HOME` is unset.
///
/// An empty variable counts as unset: an empty `OPENCOMPANY_DATA_DIR` would
/// otherwise root the instance at the process working directory.
///
/// # Errors
///
/// Fails when [`REMOVED_HOME_ENV`] is set — including when `--home` is passed,
/// because a caller who exported it believes it is placing the data and must be
/// told otherwise before a stale deploy script silently splits a store.
/// Ignoring it is what made this class of mistake cost an hour rather than a
/// minute: several hosts started with different values all shared one store, and
/// the contaminated roster read as a product bug rather than a configuration
/// one.
pub fn resolve_home(flag: Option<PathBuf>) -> Result<PathBuf> {
    resolve_home_from(
        flag,
        std::env::var_os(DATA_DIR_ENV),
        std::env::var_os(REMOVED_HOME_ENV),
        std::env::var_os("HOME"),
    )
}

/// Pure core of [`resolve_home`], taking raw environment values so the
/// precedence chain is tested without mutating the process environment.
fn resolve_home_from(
    flag: Option<PathBuf>,
    data_dir: Option<OsString>,
    removed_home: Option<OsString>,
    unix_home: Option<OsString>,
) -> Result<PathBuf> {
    let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());

    // Checked before the flag: a caller who exported the removed variable
    // believes it is doing something, and must hear otherwise either way.
    if set(removed_home).is_some() {
        return Err(OpenCompanyError::Config(format!(
            "{REMOVED_HOME_ENV} is not read by OpenCompany. Unset it and use \
             {DATA_DIR_ENV} (or the --home flag) to place the instance data root."
        )));
    }
    if let Some(flag) = flag {
        return Ok(flag);
    }
    if let Some(dir) = set(data_dir) {
        return Ok(PathBuf::from(dir));
    }
    Ok(match set(unix_home) {
        Some(home) => PathBuf::from(home)
            .join(".opencompany")
            .join(LEGACY_DEFAULT_LEAF),
        None => PathBuf::from(".opencompany").join(LEGACY_DEFAULT_LEAF),
    })
}

/// The operator warning for the one split that survives this resolution: an
/// explicit `--home` puts company bundles in one place while the instance
/// workspace (`memory/`, `store/`, `files/`, `logs/`, `tmp/`) stays under the
/// data root. Isolating two hosts with `--home` alone therefore only half-works
/// — the bundles separate, the workspace does not.
///
/// `data_root` is [`data_dir_from_env`](crate::app::config::data_dir_from_env).
/// Silent in the two aligned shapes:
///
/// - `home == data_root` — a hosted tenant (`--home "$OPENCOMPANY_DATA_DIR"`)
///   or `OPENCOMPANY_DATA_DIR` alone.
/// - `home == data_root/companies` — the untouched legacy default, which
///   diverges by design (see [`LEGACY_DEFAULT_LEAF`]) and must not warn on
///   every ordinary run.
pub fn home_divergence_warning(home: &Path, data_root: &Path) -> Option<String> {
    if home == data_root || home == data_root.join(LEGACY_DEFAULT_LEAF) {
        return None;
    }
    Some(format!(
        "company bundles resolve under {}, but the instance workspace (memory/, \
         store/, files/, logs/, tmp/) resolves under {}. Two hosts isolated by \
         --home alone still share that workspace — set {DATA_DIR_ENV} instead, \
         or to the same path, to keep an instance in one place.",
        home.display(),
        data_root.display(),
    ))
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

    /// Path to the append-only inbox log (all inboxes interleaved, one JSON
    /// email per line).
    pub fn inbox_jsonl(&self) -> PathBuf {
        self.dir.join("inbox.jsonl")
    }

    /// Path to the inbox metadata map (`key` → non-secret [`InboxMeta`], one
    /// JSON object per inbox key).
    pub fn inbox_meta_json(&self) -> PathBuf {
        self.dir.join("inbox-meta.json")
    }

    /// Path to the task board (`tasks.json`, the full board as a JSON array).
    pub fn tasks_json(&self) -> PathBuf {
        self.dir.join("tasks.json")
    }

    /// Path to the durable facts log (`facts.jsonl`, one fact per line;
    /// last-write-wins per id).
    pub fn facts_jsonl(&self) -> PathBuf {
        self.dir.join("facts.jsonl")
    }

    /// Path to the versioned task-artifact log (`artifacts.jsonl`, one artifact
    /// per line with its full version history; last-write-wins per id).
    pub fn artifacts_jsonl(&self) -> PathBuf {
        self.dir.join("artifacts.jsonl")
    }

    /// Path to the task-run log (`runs.jsonl`, one [`RunRecord`] per line;
    /// last-write-wins per id).
    ///
    /// One shared log rather than a file per run: a run id would otherwise
    /// become a path component, and a store must never let an id it did not
    /// mint address the filesystem.
    ///
    /// [`RunRecord`]: crate::ports::runs::RunRecord
    pub fn runs_jsonl(&self) -> PathBuf {
        self.dir.join("runs.jsonl")
    }

    /// Path to the run step traces (`run-steps.jsonl`, one
    /// [`RunStepRecord`] per line; last-write-wins per `(run_id, step_seq)`).
    ///
    /// Separate from `runs.jsonl` so a step is a **true append** — the run row
    /// mutates on every transition and is rewritten, but a trace only ever
    /// grows, and rewriting the whole trace per step would make a long attempt
    /// quadratic.
    ///
    /// [`RunStepRecord`]: crate::ports::runs::RunStepRecord
    pub fn run_steps_jsonl(&self) -> PathBuf {
        self.dir.join("run-steps.jsonl")
    }

    /// Path to the human user directory (`users.json`, the full set as a JSON
    /// array).
    pub fn users_json(&self) -> PathBuf {
        self.dir.join("users.json")
    }

    /// Path to the outstanding user invites (`user-invites.json`).
    pub fn user_invites_json(&self) -> PathBuf {
        self.dir.join("user-invites.json")
    }

    /// Path to the live browser sessions (`user-sessions.json`).
    ///
    /// Credential material: holds session token *hashes*. Excluded from
    /// bundle exports, like `secrets/`.
    pub fn user_sessions_json(&self) -> PathBuf {
        self.dir.join("user-sessions.json")
    }

    /// Path to the pending magic-link codes (`login-codes.json`).
    ///
    /// Credential material: holds login code *hashes*. Excluded from bundle
    /// exports, like `secrets/`.
    pub fn login_codes_json(&self) -> PathBuf {
        self.dir.join("login-codes.json")
    }

    /// Path to the usage-sample log (`usage.jsonl`, one sample per line).
    pub fn usage_jsonl(&self) -> PathBuf {
        self.dir.join("usage.jsonl")
    }

    /// Path to the skill-state deltas (`skills.json`, the full delta set).
    pub fn skills_json(&self) -> PathBuf {
        self.dir.join("skills.json")
    }

    /// The workspace subdirectory holding the seeded/edited file tree.
    pub fn workspace_dir(&self) -> PathBuf {
        self.dir.join("workspace")
    }

    /// Path to the workspace ULID → relative-path index
    /// (`.workspace-index.json`).
    pub fn workspace_index_json(&self) -> PathBuf {
        self.workspace_dir().join(".workspace-index.json")
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

    /// `resolve_home_from` with only the values a case cares about.
    fn resolve(flag: Option<&str>, data_dir: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
        resolve_home_from(
            flag.map(PathBuf::from),
            data_dir.map(OsString::from),
            None,
            home.map(OsString::from),
        )
    }

    #[test]
    fn the_flag_outranks_the_data_dir_variable() {
        // An explicit --home is never overridden by the environment.
        assert_eq!(
            resolve(Some("/flag"), Some("/env"), Some("/home/u")).unwrap(),
            PathBuf::from("/flag")
        );
    }

    #[test]
    fn the_data_dir_variable_outranks_the_default() {
        // The bug this file exists to fix: OPENCOMPANY_DATA_DIR is read, and
        // used verbatim so it matches the `--home "$OPENCOMPANY_DATA_DIR"` a
        // hosted tenant's entrypoint passes.
        assert_eq!(
            resolve(None, Some("/data"), Some("/home/u")).unwrap(),
            PathBuf::from("/data")
        );
        // Verbatim means bundles land at <root>/companies/<slug>, i.e. exactly
        // DataLayout::companies_dir() — one root for the whole instance.
        let bundle = Bundle::new(
            resolve(None, Some("/data"), Some("/home/u")).unwrap(),
            &CompanyId::new("acme"),
        );
        assert_eq!(bundle.dir(), Path::new("/data/companies/acme"));
    }

    #[test]
    fn the_default_is_unchanged_when_nothing_is_set() {
        // Existing local installs must not silently relocate: the legacy
        // default keeps its extra `companies` level (see LEGACY_DEFAULT_LEAF).
        assert_eq!(
            resolve(None, None, Some("/home/u")).unwrap(),
            PathBuf::from("/home/u/.opencompany/companies")
        );
        let bundle = Bundle::new(
            resolve(None, None, Some("/home/u")).unwrap(),
            &CompanyId::new("acme"),
        );
        assert_eq!(
            bundle.dir(),
            Path::new("/home/u/.opencompany/companies/companies/acme")
        );
        // No $HOME keeps the relative fallback.
        assert_eq!(
            resolve(None, None, None).unwrap(),
            PathBuf::from(".opencompany/companies")
        );
    }

    #[test]
    fn an_empty_data_dir_counts_as_unset() {
        // Empty would otherwise root the instance at the working directory.
        assert_eq!(
            resolve(None, Some(""), Some("/home/u")).unwrap(),
            PathBuf::from("/home/u/.opencompany/companies")
        );
        assert_eq!(
            resolve(None, Some(""), Some("")).unwrap(),
            PathBuf::from(".opencompany/companies")
        );
    }

    #[test]
    fn the_removed_home_variable_fails_loudly() {
        let err = resolve_home_from(
            None,
            None,
            Some(OsString::from("/custom/home")),
            Some(OsString::from("/home/u")),
        )
        .expect_err("OPENCOMPANY_HOME must not be silently ignored");
        let message = err.to_string();
        assert!(message.contains(REMOVED_HOME_ENV), "{message}");
        assert!(
            message.contains(DATA_DIR_ENV),
            "names the real knob: {message}"
        );

        // Loud even alongside an explicit --home, so the mistaken belief that
        // the variable does something is always corrected.
        assert!(
            resolve_home_from(
                Some(PathBuf::from("/flag")),
                None,
                Some(OsString::from("/custom/home")),
                None,
            )
            .is_err()
        );

        // An empty value is not "set" and stays silent.
        assert!(resolve_home_from(None, None, Some(OsString::new()), None).is_ok());
    }

    #[test]
    fn aligned_roots_never_warn() {
        // Hosted: the entrypoint passes --home "$OPENCOMPANY_DATA_DIR", and
        // OPENCOMPANY_DATA_DIR alone lands the same way.
        assert!(
            home_divergence_warning(Path::new("/data"), Path::new("/data")).is_none(),
            "one root for the whole instance is the intended shape"
        );
        // The untouched legacy default diverges by design and must stay silent
        // on every ordinary local run.
        assert!(
            home_divergence_warning(
                Path::new("/home/u/.opencompany/companies"),
                Path::new("/home/u/.opencompany"),
            )
            .is_none()
        );
    }

    #[test]
    fn a_split_instance_warns_with_both_roots_named() {
        // `--home` disagreeing with a set OPENCOMPANY_DATA_DIR.
        let warning = home_divergence_warning(Path::new("/flag"), Path::new("/data"))
            .expect("a disagreeing flag and data root must warn");
        assert!(warning.contains("/flag"), "{warning}");
        assert!(warning.contains("/data"), "{warning}");

        // `--home` alone, with the data root left at its default: the bundles
        // separate but the shared workspace does not, which is the half-working
        // isolation that made this bug expensive.
        let warning =
            home_divergence_warning(Path::new("/tmp/oc-a"), Path::new("/home/u/.opencompany"))
                .expect("--home alone leaves the workspace shared and must warn");
        assert!(warning.contains("/tmp/oc-a"), "{warning}");
        assert!(warning.contains("/home/u/.opencompany"), "{warning}");
    }
}
