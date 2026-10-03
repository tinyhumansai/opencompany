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
//! the only place that decision is made. It resolves to the instance workspace
//! root in every branch, so the single `companies/` segment above is the only
//! one; installs predating that carry an extra level and are moved up on boot by
//! [`migrate_legacy_nest`](crate::store::migrate::migrate_legacy_nest).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

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
/// 3. **`$HOME/.opencompany`**, the local default. Falls back to a relative
///    `.opencompany` when `$HOME` is unset.
///
/// All three branches resolve the home to the *workspace root*, so bundles land
/// at `<root>/companies/<slug>` in every case — the layout
/// [`DataLayout::companies_dir`](crate::store::DataLayout::companies_dir) and
/// `docs/spec/runtime/storage.md` document. The default used to append a
/// `companies` leaf of its own on top of the one [`Bundle::new`] adds, nesting a
/// default local install's bundles at `~/.opencompany/companies/companies/<slug>`.
/// That leaf is gone; existing installs are moved up on first launch by
/// [`migrate_legacy_nest`](crate::store::migrate::migrate_legacy_nest), which `serve`,
/// `export`, and `import` all run before touching the home.
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
        std::env::var_os("USERPROFILE"),
    )
}

/// Pure core of [`resolve_home`], taking raw environment values so the
/// precedence chain is tested without mutating the process environment.
fn resolve_home_from(
    flag: Option<PathBuf>,
    data_dir: Option<OsString>,
    removed_home: Option<OsString>,
    unix_home: Option<OsString>,
    windows_profile: Option<OsString>,
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
    // `USERPROFILE` after `HOME`, because Windows does not set `HOME` but some
    // shells there (git-bash, MSYS) set both — and a user who has `HOME` set
    // means it.
    //
    // Without this branch the fallback on Windows is the RELATIVE
    // `.opencompany`, which resolves against the process working directory: for
    // a double-clicked application that is wherever the launcher happened to
    // put it, quite possibly `C:\Program Files`, and quite possibly
    // unwritable. A relative data root is also silently *different* per launch,
    // which is worse than failing — two runs would use two stores.
    Ok(match set(unix_home).or_else(|| set(windows_profile)) {
        Some(home) => PathBuf::from(home).join(".opencompany"),
        None => PathBuf::from(".opencompany"),
    })
}

/// The operator warning for the one split that survives this resolution: an
/// explicit `--home` puts company bundles in one place while the instance
/// workspace (`memory/`, `store/`, `files/`, `logs/`, `tmp/`) stays under the
/// data root. Isolating two hosts with `--home` alone therefore only half-works
/// — the bundles separate, the workspace does not.
///
/// `data_root` is [`data_dir_from_env`](crate::app::config::data_dir_from_env).
/// Silent in the one aligned shape, `home == data_root`, which now covers every
/// branch of [`resolve_home`]: a hosted tenant
/// (`--home "$OPENCOMPANY_DATA_DIR"`), `OPENCOMPANY_DATA_DIR` alone, and the
/// untouched local default.
///
/// One deliberate consequence: an explicit `--home ~/.opencompany/companies`
/// recreates the old doubled shape and now warns where the legacy default was
/// silent. That is correct — the bundles really are one level away from the
/// workspace there.
pub fn home_divergence_warning(home: &Path, data_root: &Path) -> Option<String> {
    if home == data_root {
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

/// Longest percent-encoded secret-key filename component. `NAME_MAX` is 255
/// bytes on common filesystems; the budget stays clear of it even after the
/// digest suffix a long key needs.
const SECRET_FILENAME_BUDGET: usize = 200;

/// Bytes of the truncated SHA-256 digest appended to over-budget keys — 128
/// bits, i.e. 32 hex characters.
const SECRET_FILENAME_DIGEST_BYTES: usize = 16;

/// Encodes a secret key as an injective, bounded, filesystem-safe filename.
///
/// The leading `%` is the namespace seam that keeps the canonical and legacy
/// layouts apart: [`slug`] can only emit `[A-Za-z0-9._-]` (everything else
/// folds to `_`), so no legacy file can ever be a canonical filename. The old
/// `key-` prefix had no such guarantee — `key-foo` is a valid legacy slug, so
/// a canonical file for `foo` was readable as the legacy fallback of
/// `key-foo`, and writing `key-foo` deleted `foo`. Within the canonical
/// namespace:
///
/// - **`%k-`** prefixes a percent-encoded key that fits the budget. Every
///   byte has a unique encoding (`%` itself is encoded as `%25`), and
///   [`percent_encode`] keeps the output injective under the filesystem
///   normalizations that would otherwise fold distinct keys together — case
///   folding on macOS/Windows volumes, and Windows stripping a trailing
///   period — so distinct keys map to distinct filenames.
/// - **`%l-`** prefixes an over-budget key: the encoded form is truncated and
///   a digest of the *whole* key is appended, so two distinct long keys cannot
///   collide through the truncation. The distinct `l` class keeps long keys
///   structurally disjoint from short ones, so the digest only has to separate
///   long keys from each other.
fn secret_filename(key: &str) -> String {
    let encoded = percent_encode(key);
    if encoded.len() <= SECRET_FILENAME_BUDGET {
        return format!("%k-{encoded}");
    }
    let digest = secret_digest(key);
    let keep = SECRET_FILENAME_BUDGET - SECRET_FILENAME_DIGEST_BYTES * 2 - 1;
    format!("%l-{}-{digest}", &encoded[..keep])
}

/// Percent-encodes every byte outside `[a-z0-9.-]`. `%` itself is encoded so
/// the output has one parse — and one file — per byte sequence.
///
/// ASCII upper-case letters are encoded, not passed through verbatim. On a
/// case-insensitive filesystem `A` and `a` are one directory entry, so a
/// passthrough that kept `[A-Za-z0-9.-]` let two keys differing only in case —
/// two distinct MCP server names, per [`validate_servers`](crate::company::mcp)
/// — share one file and overwrite each other. Encoding every `A`–`Z` keeps the
/// output injective under case-folding: the only upper-case bytes it can ever
/// contain are the hex digits of its own `%`-escapes, which are emitted in one
/// fixed case, so no two distinct keys fold to the same filename.
///
/// A trailing `.` is encoded too. Windows Win32 paths strip trailing periods,
/// so a passthrough `.` at the end let `foo` and `foo.` resolve to the same
/// directory entry; the encoded `%2E` keeps the filename from ever ending in a
/// dot. Interior periods are unaffected — Windows only strips trailing ones.
fn percent_encode(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let bytes = key.as_bytes();
    let last = bytes.len().wrapping_sub(1);
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte == b'.' && index == last {
            out.push_str("%2E");
        } else if byte.is_ascii_digit() || byte.is_ascii_lowercase() || matches!(byte, b'.' | b'-')
        {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(out, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    out
}

/// 128-bit truncated SHA-256 of a key, hex-encoded.
fn secret_digest(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest.iter().take(SECRET_FILENAME_DIGEST_BYTES).fold(
        String::with_capacity(SECRET_FILENAME_DIGEST_BYTES * 2),
        |mut acc, byte| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{byte:02x}");
            acc
        },
    )
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

    /// The company's repository mirror cache (issue #245).
    ///
    /// The same location
    /// [`DataLayout::company_repos_dir`](crate::store::DataLayout::company_repos_dir)
    /// names — resolved through the bundle so [`slug`] is applied in exactly one
    /// place, rather than by a caller that would have to reimplement it.
    ///
    /// Note what it is *not*: bundle content. Nothing in the fs store reads or
    /// creates it, and on a mongodb tenant no other part of this directory
    /// exists at all. It shares the prefix so a company's whole footprint stays
    /// in one subtree and one quota walk.
    pub fn repos_dir(&self) -> PathBuf {
        self.dir.join("repos")
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

    /// Path to the company's ledger **declarations** (`ledgers.json`, the whole
    /// set as a JSON array).
    ///
    /// Declarations only. The built-ins ship with the runtime and are never
    /// stored, so a company's file cannot drift from the code every prompt and
    /// route is written against.
    pub fn ledgers_json(&self) -> PathBuf {
        self.dir.join("ledgers.json")
    }

    /// Path to one ledger's append-only event log
    /// (`ledgers/<slug>.jsonl`, one [`LedgerEvent`](crate::ledger::LedgerEvent)
    /// per line, in append order).
    ///
    /// One file per ledger rather than one shared log: the fold reads a single
    /// ledger at a time, and a shared file would make every read of the goals
    /// scan every task event ever written. Concurrent writers to *different*
    /// ledgers then never touch the same file at all, and writers to the same
    /// one interleave whole lines under `O_APPEND`.
    ///
    /// The slug is `[a-z0-9-]` by construction
    /// ([`normalize_slug`](crate::ledger::normalize_slug)), so it cannot escape
    /// this directory.
    pub fn ledger_events_jsonl(&self, slug: &str) -> PathBuf {
        self.ledgers_dir().join(format!("{slug}.jsonl"))
    }

    /// Path to the per-ledger event log directory.
    pub fn ledgers_dir(&self) -> PathBuf {
        self.dir.join("ledgers")
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

    /// Path to the per-workflow revision ring (`workflow-revisions.jsonl`, one
    /// [`WorkflowRevisionRecord`] per line, append-then-prune to the cap; issue
    /// #274). Not last-write-wins: every distinct snapshot is its own immutable
    /// line, keyed by its minted `id`.
    ///
    /// [`WorkflowRevisionRecord`]: crate::ports::workflow_revisions::WorkflowRevisionRecord
    pub fn workflow_revisions_jsonl(&self) -> PathBuf {
        self.dir.join("workflow-revisions.jsonl")
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

    /// Path to the per-node run-output log (`run-outputs.jsonl`, one
    /// [`WorkflowRunOutputRecord`] per line; last-write-wins per `run_id`,
    /// prune-to-newest-N per company; issue #596).
    ///
    /// One shared log rather than a file per run — the same rule
    /// [`runs_jsonl`](Self::runs_jsonl) states: a run id is caller-minted and must
    /// never become a path component the store did not mint.
    ///
    /// [`WorkflowRunOutputRecord`]: crate::ports::run_output::WorkflowRunOutputRecord
    pub fn run_outputs_jsonl(&self) -> PathBuf {
        self.dir.join("run-outputs.jsonl")
    }

    /// Path to the unredacted step-detail log (`deep-trace.jsonl`, one
    /// [`RunStepDetailRecord`] per line; last-write-wins per
    /// `(run_id, step_seq)`, prune-to-newest-N runs per company).
    ///
    /// A sibling of [`run_steps_jsonl`](Self::run_steps_jsonl) rather than a
    /// widening of it: the skeleton there is safe to render anywhere, this holds
    /// raw arguments and raw output, and keeping them in separate files is what
    /// lets the bodies be purged without touching run history.
    ///
    /// One shared log rather than a file per run, for the same reason its
    /// siblings are: a run id is caller-minted and must never become a path
    /// component the store did not mint.
    ///
    /// [`RunStepDetailRecord`]: crate::ports::deep_trace::RunStepDetailRecord
    pub fn deep_trace_jsonl(&self) -> PathBuf {
        self.dir.join("deep-trace.jsonl")
    }

    /// The per-company schedule-fire claim subdirectory
    /// (`schedule_fires/<hashed-schedule-id>/<minute>`, one empty-ish marker
    /// file per claimed instant; #241).
    ///
    /// A directory of `O_EXCL` marker files rather than a JSONL log: the claim's
    /// whole value is that `create_new` is atomic, so the *existence* of the
    /// file is the claim. The schedule-id component is hashed before it becomes a
    /// path so an id the store did not mint can never address the filesystem —
    /// the same rule [`runs_jsonl`](Self::runs_jsonl) states for run ids.
    pub fn schedule_fires_dir(&self) -> PathBuf {
        self.dir.join("schedule_fires")
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

    /// Path to the per-person channel read markers (`read-state.json`, #755).
    pub fn read_state_json(&self) -> PathBuf {
        self.dir.join("read-state.json")
    }

    /// Path to the durable notification records (`notifications.json`, #749).
    pub fn notifications_json(&self) -> PathBuf {
        self.dir.join("notifications.json")
    }

    /// Path to the per-person notification read markers
    /// (`notification-reads.json`, #749) — kept beside the records rather than
    /// on them, because read state is per person, not per company.
    pub fn notification_reads_json(&self) -> PathBuf {
        self.dir.join("notification-reads.json")
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
    ///
    /// Canonical filenames always start with `%` (see [`secret_filename`]), so
    /// they are disjoint from every legacy slugged filename in the same
    /// directory.
    pub fn secret(&self, key: &str) -> PathBuf {
        self.secrets_dir().join(secret_filename(key))
    }

    /// Path used for secrets before secret filenames became injective.
    ///
    /// [`FsSecretStore`](crate::store::FsSecretStore) reads this path only as a
    /// migration fallback. It is never written by the new layout, and its
    /// `[A-Za-z0-9._-]` filename can never coincide with a canonical
    /// `%`-prefixed one. [`set`](crate::ports::SecretStore::set) deliberately
    /// keeps the file: one slug can name several distinct keys, and an
    /// un-migrated colliding alias still reads it through the fallback. `get`
    /// prefers the canonical file, so the kept legacy file is shadowed for the
    /// migrated key.
    pub(crate) fn legacy_secret(&self, key: &str) -> PathBuf {
        self.secrets_dir().join(slug(&CompanyId::new(key)))
    }

    /// The per-company key material subdirectory (`0700` on unix).
    pub fn keys_dir(&self) -> PathBuf {
        self.dir.join("keys")
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

#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;
