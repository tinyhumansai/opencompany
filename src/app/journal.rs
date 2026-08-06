//! Where the embedded OpenHuman runtime keeps its durable agent journal.
//!
//! The vendored runtime writes every agent observation under
//! `{workspace}/tinyagents_store/`, and it resolves `{workspace}` from *its
//! own* config — which, absent an override, is a subdirectory of the user's
//! home directory. In a hosted tenant that home directory sits on the
//! **read-only root filesystem**: the store's first `create_dir_all` fails with
//! `EROFS`, and the vendored append worker reports the failure once per queued
//! event, forever, on stderr. Nothing else in the container's log survives that
//! (issue #446).
//!
//! OpenCompany's writable per-instance root is the data directory
//! ([`data_dir_from_env`](super::config::data_dir_from_env) — `/data` in a
//! tenant, where the manager mounts the persistent volume). This module points
//! the vendored resolver there through the one seam it honours — the
//! `OPENHUMAN_WORKSPACE` environment variable, which its config loader consults
//! *before* any home-directory default — and proves the resulting root is
//! writable while still in `serve`'s startup path.
//!
//! Two properties follow from doing the check at startup:
//!
//! * **It is loud once, not quiet forever.** A root that cannot be created is a
//!   misconfiguration the operator must see immediately, so [`prepare`] returns
//!   an error and `serve` aborts rather than running an agent whose work is
//!   never recorded.
//! * **The per-append flood cannot begin.** The vendored append worker has no
//!   dedup or backoff and reports straight to stderr, so no log filter on this
//!   side can bound it. Refusing to boot means the loop is never entered — the
//!   condition it would retry against cannot resolve itself while the process
//!   lives, because the mount is read-only for the lifetime of the pod.
//!
//! Only the resolved *path* is reported at startup. The only environment values
//! read here are `OPENHUMAN_WORKSPACE` and — for the temp-directory guard below
//! — the platform's temp-dir variable, and neither is echoed, so the startup
//! line cannot carry credential material.
//!
//! # The keyring rides the same root
//!
//! `pin_keyring` registers the resolved root as the vendored runtime's
//! *keyring* directory too, so secret material lands beside the journal rather
//! than in whatever its own fallback chain reaches — a chain that ends, if no
//! home directory resolves, at `/tmp` with no log line at all (issue #451). The
//! export above happens to steer it correctly today, but only because nothing
//! has touched the keyring yet; registering says it outright instead of relying
//! on startup order.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::error::OpenCompanyError;

/// The vendored runtime's workspace override. Its config loader reads this
/// before consulting the home-directory default, which makes it the only seam
/// a host process has for redirecting the journal.
pub const OPENHUMAN_WORKSPACE_ENV: &str = "OPENHUMAN_WORKSPACE";

/// The data-dir subdirectory handed to the vendored runtime as its root.
const OPENHUMAN_SUBDIR: &str = "openhuman";

/// The workspace subdirectory the vendored resolver appends to the root.
const WORKSPACE_SUBDIR: &str = "workspace";

/// The store subtree the journal and kv stores live in, under the workspace.
const STORE_SUBDIR: &str = "tinyagents_store";

/// Payload for the startup write probe. Non-empty on purpose — see
/// [`ensure_writable`].
const PROBE_BYTES: &[u8] = b"opencompany journal write probe\n";

/// Where a resolved journal root came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootSource {
    /// An operator set `OPENHUMAN_WORKSPACE`; its value is used verbatim.
    Env,
    /// Derived from the instance data directory — the normal hosted path.
    DataDir,
}

impl RootSource {
    /// The knob an operator would change to move the root, for the startup line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Env => OPENHUMAN_WORKSPACE_ENV,
            Self::DataDir => "OPENCOMPANY_DATA_DIR",
        }
    }
}

/// A resolved (but not yet proven-writable) journal location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalRoot {
    root: PathBuf,
    source: RootSource,
}

impl JournalRoot {
    /// The directory handed to the vendored runtime as `OPENHUMAN_WORKSPACE`.
    /// Everything the journal writes lands beneath it, so this is the path
    /// whose writability decides whether observations persist.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Which knob put the root where it is.
    pub fn source(&self) -> RootSource {
        self.source
    }

    /// The store subtree the journal is expected to open,
    /// `<root>/workspace/tinyagents_store`.
    ///
    /// This mirrors the vendored resolver's ordinary result. That resolver has
    /// one legacy branch which, when a sibling `<root>/../.openhuman/config.toml`
    /// exists, treats `<root>` *itself* as the workspace and would place the
    /// store at `<root>/tinyagents_store` instead. It requires an
    /// operator-created directory that a provisioned tenant never has, and both
    /// outcomes lie inside [`root`](Self::root) — which is why the startup probe
    /// checks the root rather than this leaf.
    pub fn store_root(&self) -> PathBuf {
        self.root.join(WORKSPACE_SUBDIR).join(STORE_SUBDIR)
    }

    /// The one-line startup report: where observations are being written, and
    /// which knob decided that.
    pub fn summary(&self) -> String {
        format!(
            "agent journal: {} (root {}, from {})",
            self.store_root().display(),
            self.root.display(),
            self.source.as_str()
        )
    }
}

/// Resolves the journal root from an `OPENHUMAN_WORKSPACE` value and the
/// instance data directory, without touching the filesystem or the environment.
///
/// An operator-set root wins so a self-hoster keeps an existing OpenHuman
/// workspace. A missing — or blank — value falls back to the data directory,
/// the one location a tenant container is guaranteed to be able to write.
///
/// Trimming decides only whether a value is blank; a value that survives that
/// test is used **verbatim**. Leading and trailing spaces are legal in a POSIX
/// path, so silently stripping them would resolve somewhere the operator did
/// not configure — the same class of surprise this module exists to remove.
pub fn resolve(env_value: Option<&str>, data_dir: &Path) -> JournalRoot {
    match env_value.filter(|value| !value.trim().is_empty()) {
        Some(value) => JournalRoot {
            root: PathBuf::from(value),
            source: RootSource::Env,
        },
        None => JournalRoot {
            root: data_dir.join(OPENHUMAN_SUBDIR),
            source: RootSource::DataDir,
        },
    }
}

/// Resolves the journal root and proves it is writable, without reading or
/// writing the process environment.
///
/// The pure-input half of [`prepare`], so the resolution and the probe can be
/// exercised without the `set_var` races that make environment mutation
/// unsuitable for parallel tests.
pub async fn prepare_with(env_value: Option<&str>, data_dir: &Path) -> Result<JournalRoot> {
    let resolved = resolve(env_value, data_dir);
    ensure_writable(resolved.root(), resolved.source()).await?;
    Ok(resolved)
}

/// Resolves the journal root, proves it is writable, and — when the root was
/// derived rather than supplied — exports it as `OPENHUMAN_WORKSPACE` so the
/// vendored config loader finds it instead of a home-directory default.
///
/// Returns the resolved root for the caller to report. An unwritable root is an
/// error: see the module docs for why that aborts startup instead of degrading.
///
/// Call this once, early in `serve`, before any company runtime is built — the
/// vendored loader reads the variable when the first agent harness is
/// constructed, and the export must already have happened.
pub async fn prepare(data_dir: &Path) -> Result<JournalRoot> {
    let raw = std::env::var(OPENHUMAN_WORKSPACE_ENV).ok();
    let resolved = prepare_with(raw.as_deref(), data_dir).await?;

    if resolved.source() == RootSource::DataDir {
        // SAFETY: `serve` calls this once during startup, before any company
        // runtime, scheduler, mailbox poller or HTTP listener exists, so no
        // other thread is reading the environment concurrently. The tokio
        // worker and blocking threads alive at this point are idle and do not
        // call `getenv`. Setting this later — from a request handler, say —
        // would race those readers and must not be done.
        unsafe { std::env::set_var(OPENHUMAN_WORKSPACE_ENV, resolved.root()) };
    }

    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Keyring pin (issue #451)
// ---------------------------------------------------------------------------

/// Where the vendored keyring writes its secret material, once [`pin_keyring`]
/// has registered it.
#[cfg(feature = "openhuman")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyringPin {
    dir: PathBuf,
    source: RootSource,
    temporary: bool,
}

#[cfg(feature = "openhuman")]
impl KeyringPin {
    /// The directory the vendored keyring will resolve to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the pinned directory sits under the system temp directory — see
    /// [`pin_keyring`] for why that is reported rather than refused.
    pub fn is_temporary(&self) -> bool {
        self.temporary
    }

    /// The one-line startup report: which directory holds the credentials, and
    /// the caveat when that directory is temporary.
    pub fn summary(&self) -> String {
        let base = format!(
            "keyring: {} (pinned, from {})",
            self.dir.display(),
            self.source.as_str()
        );
        if self.temporary {
            format!(
                "{base} — WARNING: this is under the system temp directory. \
                 Credentials stored here can be removed by the OS at any time, \
                 and connected accounts will silently need reconnecting. Point \
                 {} at a durable volume for anything but local development.",
                self.source.as_str()
            )
        } else {
            base
        }
    }
}

/// Whether `dir` lies inside `temp_dir`.
///
/// Component-wise by construction ([`Path::starts_with`] compares whole
/// components), so `/tmpfoo` is not treated as being under `/tmp`.
#[cfg(feature = "openhuman")]
fn under_temp_dir(dir: &Path, temp_dir: &Path) -> bool {
    dir.starts_with(temp_dir)
}

/// Registers `root` as the vendored keyring's workspace, so credential storage
/// lands beside the journal instead of wherever its own fallback chain ends up.
///
/// # Why register rather than rely on the export
///
/// The vendored resolver
/// (`keyring::store::workspace_dir_for_file_backend`) tries, in order: a
/// directory registered through its `init_workspace` seam, then
/// `OPENHUMAN_WORKSPACE`, then the user's home directory — and if it cannot
/// find a home directory at all, `/tmp`, at no log level whatsoever.
///
/// [`prepare`] already exports `OPENHUMAN_WORKSPACE`, so today a hosted tenant
/// happens to land in the data dir. That is safe **by accident**: the resolved
/// value is cached in a `OnceLock`, so whichever code touches the keyring first
/// fixes the answer for the life of the process. Nothing enforces that the
/// export happens before that first touch — it holds because of the order two
/// unrelated pieces of startup currently run in, which is not a property anyone
/// is checking when they move code around. If a touch ever lands first, secret
/// material silently goes to `$HOME` (the read-only root filesystem in a tenant)
/// or to `/tmp`, with no error and no log line to say so.
///
/// Registering explicitly removes the ordering dependency instead of restating
/// it: after this call the first branch of the resolver matches, so the env-var
/// branch, the home branch and the `/tmp` branch are all unreachable regardless
/// of what runs when. `init_workspace` is idempotent-by-first-writer (it logs at
/// debug and ignores a second call), and this is called once from `serve`.
///
/// # Loudness, not refusal
///
/// A pinned root under the system temp directory earns a `warn!` naming the path
/// and what it costs — not a refusal. Pointing `OPENCOMPANY_DATA_DIR` at a temp
/// path is a legitimate thing to do in local development and in tests; the
/// defect this addresses was never that credentials *could* be temporary, it was
/// that they could become temporary in **silence**.
///
/// # Owed upstream
///
/// The `/tmp` fallback itself still exists in `vendor/openhuman` and should be
/// removed there — a keyring that cannot resolve a durable directory should
/// fail loudly rather than quietly choosing world-readable scratch space. That
/// is a vendored-crate change and is out of scope here; this function makes the
/// fallback unreachable for `opencompany serve`, which is the half this crate
/// can own.
#[cfg(feature = "openhuman")]
pub fn pin_keyring(root: &JournalRoot) -> KeyringPin {
    let dir = root.root().to_path_buf();
    openhuman_core::openhuman::keyring::init_workspace(&dir);

    let temporary = under_temp_dir(&dir, &std::env::temp_dir());
    if temporary {
        tracing::warn!(
            keyring = %dir.display(),
            knob = root.source().as_str(),
            "[keyring] secret material is being stored under the system temp directory; \
             the OS may remove it at any time and connected accounts will silently need \
             reconnecting — point the data directory at a durable volume outside local development",
        );
    }

    KeyringPin {
        dir,
        source: root.source(),
        temporary,
    }
}

/// Creates `root` and confirms a file can actually be written inside it.
///
/// `create_dir_all` alone is not proof: it succeeds silently when the directory
/// already exists but is not writable by this user, which is exactly the shape
/// a stale volume or a wrong `fsGroup` produces. The probe file is named per
/// process so two instances sharing a root cannot collide, and is removed
/// again — a failure to remove it is not fatal, since the write already proved
/// the point.
///
/// The probe carries real bytes rather than being empty. A zero-length write
/// allocates no blocks, so it succeeds on a volume that is full or over quota
/// while every subsequent journal append fails with `ENOSPC` — reintroducing
/// the per-append failure this module exists to make impossible.
async fn ensure_writable(root: &Path, source: RootSource) -> Result<()> {
    let fail = |stage: &str, error: std::io::Error| {
        OpenCompanyError::Config(format!(
            "agent journal root {} is not writable ({stage}: {error}); \
             refusing to start, because agent observations would be silently \
             discarded on every turn. Point {} at a writable volume — in a \
             tenant container that is the mounted data volume, not the \
             read-only root filesystem.",
            root.display(),
            source.as_str()
        ))
    };

    tokio::fs::create_dir_all(root)
        .await
        .map_err(|error| fail("create", error))?;

    let probe = root.join(format!(".opencompany-journal-probe-{}", std::process::id()));
    tokio::fs::write(&probe, PROBE_BYTES)
        .await
        .map_err(|error| fail("write", error))?;
    tokio::fs::remove_file(&probe).await.ok();

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    fn scratch_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("oc-journal-{}-{tag}", std::process::id()))
    }

    /// The child test the seam test re-invokes this binary to run.
    #[cfg(feature = "openhuman")]
    const SEAM_CHILD_TEST: &str = "app::journal::test::vendored_seam_child";

    /// Which half of the seam the child should assert.
    #[cfg(feature = "openhuman")]
    const SEAM_ROLE_ENV: &str = "OC_JOURNAL_SEAM_ROLE";

    /// The data dir the child compares against.
    #[cfg(feature = "openhuman")]
    const SEAM_DATA_DIR_ENV: &str = "OC_JOURNAL_SEAM_DATA_DIR";

    /// The child test the keyring-pin test re-invokes this binary to run.
    #[cfg(feature = "openhuman")]
    const KEYRING_CHILD_TEST: &str = "app::journal::test::keyring_pin_child";

    /// The data dir the keyring-pin child pins against.
    #[cfg(feature = "openhuman")]
    const KEYRING_DATA_DIR_ENV: &str = "OC_KEYRING_PIN_DATA_DIR";

    #[test]
    fn absent_env_roots_the_journal_in_the_data_dir() {
        let resolved = resolve(None, Path::new("/data"));

        assert_eq!(resolved.root(), Path::new("/data/openhuman"));
        assert_eq!(resolved.source(), RootSource::DataDir);
        assert_eq!(
            resolved.store_root(),
            Path::new("/data/openhuman/workspace/tinyagents_store"),
            "the store must land inside the mounted data volume, not $HOME"
        );
    }

    #[test]
    fn an_operator_supplied_root_wins() {
        let resolved = resolve(Some("/srv/openhuman"), Path::new("/data"));

        assert_eq!(resolved.root(), Path::new("/srv/openhuman"));
        assert_eq!(resolved.source(), RootSource::Env);
        assert_eq!(
            resolved.store_root(),
            Path::new("/srv/openhuman/workspace/tinyagents_store")
        );
    }

    #[test]
    fn a_padded_root_is_used_exactly_as_configured() {
        // Spaces are legal in a POSIX path, so a value that is not blank must
        // survive verbatim — trimming it would resolve somewhere the operator
        // never named.
        for padded in [" /srv/oh ", "/srv/oh ", " /srv/oh", "/srv/my oh"] {
            let resolved = resolve(Some(padded), Path::new("/data"));
            assert_eq!(
                resolved.root(),
                Path::new(padded),
                "{padded:?} must not be rewritten"
            );
            assert_eq!(resolved.source(), RootSource::Env);
        }
    }

    #[test]
    fn blank_env_values_fall_back_to_the_data_dir() {
        // The vendored resolver ignores an empty value but would honour a
        // whitespace-only one as a relative path; treating both as unset keeps
        // a shell that exports a declared-but-unset variable out of $HOME.
        for blank in ["", "   ", "\t"] {
            let resolved = resolve(Some(blank), Path::new("/data"));
            assert_eq!(
                resolved.root(),
                Path::new("/data/openhuman"),
                "{blank:?} should not be taken as a root"
            );
            assert_eq!(resolved.source(), RootSource::DataDir);
        }
    }

    #[test]
    fn the_summary_names_the_store_and_the_knob() {
        let summary = resolve(None, Path::new("/data")).summary();

        assert!(summary.contains("/data/openhuman/workspace/tinyagents_store"));
        assert!(summary.contains("OPENCOMPANY_DATA_DIR"));

        let env_summary = resolve(Some("/srv/oh"), Path::new("/data")).summary();
        assert!(env_summary.contains(OPENHUMAN_WORKSPACE_ENV));
    }

    #[tokio::test]
    async fn a_writable_data_dir_prepares_and_leaves_no_probe_behind() {
        let root = scratch_root("writable");
        tokio::fs::create_dir_all(&root).await.unwrap();

        let resolved = prepare_with(None, &root).await.unwrap();
        assert_eq!(resolved.root(), root.join("openhuman"));
        assert!(resolved.root().is_dir(), "the root is created, not assumed");

        let mut entries = tokio::fs::read_dir(resolved.root()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name();
            assert!(
                !name.to_string_lossy().contains("probe"),
                "the write probe must be cleaned up, found {name:?}"
            );
        }

        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn preparing_twice_is_idempotent() {
        let root = scratch_root("idempotent");
        tokio::fs::create_dir_all(&root).await.unwrap();

        let first = prepare_with(None, &root).await.unwrap();
        let second = prepare_with(None, &root).await.unwrap();
        assert_eq!(first, second, "a restart must reuse the same root");

        tokio::fs::remove_dir_all(&root).await.ok();
    }

    /// The read-only-root condition from issue #446, reproduced with an
    /// unwritable parent: startup must fail with a message that names the path
    /// and the knob, rather than letting the runtime discover it per append.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritable_root_fails_loudly_at_startup() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch_root("readonly");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let mut perms = tokio::fs::metadata(&root).await.unwrap().permissions();
        perms.set_mode(0o500);
        tokio::fs::set_permissions(&root, perms).await.unwrap();

        let outcome = prepare_with(None, &root).await;

        // A root user ignores the mode bits, so the probe would succeed and
        // there is nothing to assert about. Restore and skip rather than fail.
        let Err(error) = outcome else {
            restore_and_remove(&root).await;
            return;
        };
        let message = error.to_string();

        assert!(
            message.contains("openhuman"),
            "the failure must name the path: {message}"
        );
        assert!(
            message.contains("OPENCOMPANY_DATA_DIR"),
            "the failure must name the knob to change: {message}"
        );
        assert!(
            message.contains("refusing to start"),
            "the failure must say the instance will not run degraded: {message}"
        );

        restore_and_remove(&root).await;
    }

    /// A root that exists but cannot be written to is the shape a stale volume
    /// or a wrong `fsGroup` produces; `create_dir_all` alone reports success.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_existing_but_unwritable_root_is_caught_by_the_write_probe() {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = scratch_root("existing-ro");
        let root = data_dir.join("openhuman");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let mut perms = tokio::fs::metadata(&root).await.unwrap().permissions();
        perms.set_mode(0o500);
        tokio::fs::set_permissions(&root, perms).await.unwrap();

        let outcome = prepare_with(None, &data_dir).await;

        // As above: a root user is not bound by the mode bits.
        if let Err(error) = outcome {
            assert!(
                error.to_string().contains("write"),
                "create_dir_all succeeds on an existing dir; the probe must be \
                 what fails: {error}"
            );
        }

        restore_and_remove(&root).await;
        tokio::fs::remove_dir_all(&data_dir).await.ok();
    }

    /// The seam itself, against the real vendored resolver.
    ///
    /// Everything else here tests our side of the contract. This asserts the
    /// other side: that [`OPENHUMAN_WORKSPACE`](OPENHUMAN_WORKSPACE_ENV) is
    /// still the name the vendored config loader reads, and that a root of the
    /// shape [`resolve`] produces still lands its workspace where
    /// [`JournalRoot::store_root`] says it will. If upstream renames the
    /// variable or changes its resolution, this fails instead of the journal
    /// silently reverting to `$HOME` in production.
    ///
    /// Run in a **child process** rather than by mutating this process's
    /// environment.
    ///
    /// `set_var` is process-global, and this library's test binary contains
    /// unguarded environment readers and writers in several other modules
    /// (`store::select`, `server::ops::connections`, `harness::composio`,
    /// `server::ops::mailer`). A lock would only protect this test if every one
    /// of those took the same lock, and nothing stops the next test from adding
    /// an unguarded `set_var`. Handing the variable to a child at spawn time
    /// removes the shared mutable state instead of scheduling around it, and
    /// exercises the more faithful thing: a process that *starts* with the
    /// environment a tenant container is given.
    #[cfg(feature = "openhuman")]
    #[test]
    fn the_vendored_loader_still_honours_the_workspace_variable() {
        let data_dir = scratch_root("vendored-seam");
        // Deliberately outside the data dir, so "did the workspace land under
        // the data dir?" cannot be satisfied by the home directory instead.
        let home = scratch_root("vendored-seam-home");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let expected = resolve(None, &data_dir);
        let run = |role: &str, export_seam: bool| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([SEAM_CHILD_TEST, "--exact", "--ignored", "--nocapture"])
                .env(SEAM_ROLE_ENV, role)
                .env(SEAM_DATA_DIR_ENV, &data_dir)
                .env("HOME", &home);
            if export_seam {
                command.env(OPENHUMAN_WORKSPACE_ENV, expected.root());
            } else {
                command.env_remove(OPENHUMAN_WORKSPACE_ENV);
            }
            command.output().expect("spawn the seam child process")
        };

        // A filter that stops matching (a rename, a moved module) makes libtest
        // run nothing and still exit 0, so success alone would be a vacuous
        // pass. Require that exactly one test actually ran.
        let check = |what: &str, out: &std::process::Output| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let detail = format!(
                "{what}\n--- child stdout ---\n{stdout}\n--- child stderr ---\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success(), "{detail}");
            assert!(
                stdout.contains("1 passed"),
                "the child must have actually run the assertion, not filtered \
                 it away — is {SEAM_CHILD_TEST} still the right path?\n{detail}"
            );
        };

        // Exporting the variable puts the vendored workspace under the data dir.
        check("the exported root must be honoured", &run("inside", true));

        // Negative control: without it, the vendored runtime goes to $HOME. If
        // this ever passes, the check above proves nothing.
        check(
            "without the variable the runtime must fall back to $HOME",
            &run("outside", false),
        );

        std::fs::remove_dir_all(&data_dir).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// The assertion half of the seam test, executed in the child process the
    /// test above spawns. Ignored so a normal run never picks it up, and inert
    /// unless the parent set the role, so a bare `cargo test -- --ignored`
    /// cannot fail on a missing environment.
    #[cfg(feature = "openhuman")]
    #[tokio::test]
    #[ignore = "spawned by the_vendored_loader_still_honours_the_workspace_variable"]
    async fn vendored_seam_child() {
        let Ok(role) = std::env::var(SEAM_ROLE_ENV) else {
            return;
        };
        let data_dir = PathBuf::from(std::env::var(SEAM_DATA_DIR_ENV).expect("parent sets this"));
        let expected = resolve(None, &data_dir);

        let config = openhuman_core::openhuman::config::Config::load_or_init()
            .await
            .expect("the vendored config must load");

        match role.as_str() {
            "inside" => {
                assert_eq!(
                    config.workspace_dir,
                    expected.root().join(WORKSPACE_SUBDIR),
                    "the vendored runtime must place its workspace under the \
                     root we export"
                );
                assert!(
                    config.workspace_dir.starts_with(&data_dir),
                    "the journal must resolve inside the data dir, got {}",
                    config.workspace_dir.display()
                );
            }
            "outside" => assert!(
                !config.workspace_dir.starts_with(&data_dir),
                "without {OPENHUMAN_WORKSPACE_ENV} the runtime must not reach \
                 the data dir on its own — the export is what does the work, \
                 got {}",
                config.workspace_dir.display()
            ),
            other => panic!("unknown seam role {other:?}"),
        }
    }

    /// The temp-dir guard compares whole components, so a sibling directory
    /// whose name merely starts with the temp dir's is not "inside" it.
    #[cfg(feature = "openhuman")]
    #[test]
    fn the_temp_dir_guard_compares_components_not_prefixes() {
        let temp = Path::new("/tmp");

        assert!(under_temp_dir(Path::new("/tmp/oc/openhuman"), temp));
        assert!(under_temp_dir(temp, temp), "the temp dir itself counts");
        assert!(
            !under_temp_dir(Path::new("/tmpfoo/openhuman"), temp),
            "a name that merely begins with the temp dir's is a different directory"
        );
        assert!(!under_temp_dir(Path::new("/data/openhuman"), temp));
        assert!(!under_temp_dir(Path::new("/var/lib/oc"), temp));
    }

    /// With `OPENHUMAN_WORKSPACE` unset, the vendored keyring must **still**
    /// resolve to the data-dir root — i.e. `$HOME` and its `/tmp` fallback are
    /// unreachable because the directory was registered, not inferred (#451).
    ///
    /// The negative half is what makes this worth running: the child's `HOME`
    /// points somewhere real and writable, so if the pin did nothing the
    /// resolver would happily answer with `$HOME/.openhuman` and the assertion
    /// would fail rather than pass vacuously.
    ///
    /// Run in a **child process** for the same reason the vendored-seam test
    /// above is: the registration is a process-global `OnceLock` set-once, so it
    /// cannot be undone between tests, and `HOME` cannot be varied in-process
    /// without racing every other environment reader in this binary.
    #[cfg(feature = "openhuman")]
    #[test]
    fn pinning_keeps_the_keyring_out_of_home_without_the_env_var() {
        let data_dir = scratch_root("keyring-pin");
        // Deliberately a sibling of the data dir, not a child, so "did the
        // keyring land under the data dir?" cannot be satisfied by $HOME.
        let home = scratch_root("keyring-pin-home");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([KEYRING_CHILD_TEST, "--exact", "--ignored", "--nocapture"])
            .env(KEYRING_DATA_DIR_ENV, &data_dir)
            .env("HOME", &home)
            // The whole point: the pin must work with the seam variable absent.
            .env_remove(OPENHUMAN_WORKSPACE_ENV)
            .output()
            .expect("spawn the keyring-pin child process");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = format!(
            "--- child stdout ---\n{stdout}\n--- child stderr ---\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "{detail}");
        // A filter that stops matching makes libtest run nothing and still exit
        // 0, so success alone would be a vacuous pass.
        assert!(
            stdout.contains("1 passed"),
            "the child must have actually run the assertion, not filtered it \
             away — is {KEYRING_CHILD_TEST} still the right path?\n{detail}"
        );

        std::fs::remove_dir_all(&data_dir).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// The assertion half of the keyring-pin test, executed in the child process
    /// the test above spawns. Ignored so a normal run never picks it up, and
    /// inert unless the parent set the data dir, so a bare
    /// `cargo test -- --ignored` cannot fail on a missing environment.
    #[cfg(feature = "openhuman")]
    #[test]
    #[ignore = "spawned by pinning_keeps_the_keyring_out_of_home_without_the_env_var"]
    fn keyring_pin_child() {
        let Ok(data_dir) = std::env::var(KEYRING_DATA_DIR_ENV) else {
            return;
        };
        let data_dir = PathBuf::from(data_dir);
        assert!(
            std::env::var(OPENHUMAN_WORKSPACE_ENV).is_err(),
            "the parent must unset the seam variable — the registration, not the \
             export, is what this test proves"
        );

        let expected = resolve(None, &data_dir);
        let pin = pin_keyring(&expected);
        assert_eq!(pin.dir(), expected.root());

        let resolved = openhuman_core::openhuman::keyring::store::workspace_dir_for_file_backend();
        assert_eq!(
            resolved,
            expected.root(),
            "the vendored keyring must answer with the registered root"
        );
        assert!(
            resolved.starts_with(&data_dir),
            "secret material must resolve inside the data dir, got {}",
            resolved.display()
        );

        // The home fallback is real and writable here, so this is the branch the
        // pin actually displaced — and `/tmp` sits one step further down the
        // same chain.
        let home = PathBuf::from(std::env::var("HOME").expect("the parent sets HOME"));
        assert!(
            !resolved.starts_with(&home),
            "the keyring must not fall back to $HOME once a root is registered"
        );

        // The data dir is scratch space under the system temp dir, so the
        // loudness guard fires — and it warns rather than refusing, which is why
        // `pin_keyring` returned at all.
        assert!(
            pin.is_temporary(),
            "a root under the system temp dir must be flagged"
        );
        assert!(
            pin.summary().contains("WARNING"),
            "the startup line must carry the caveat: {}",
            pin.summary()
        );
    }

    /// Puts an owner-writable mode back on `dir` so the test's own cleanup can
    /// remove it.
    #[cfg(unix)]
    async fn restore_and_remove(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;

        if let Ok(metadata) = tokio::fs::metadata(dir).await {
            let mut perms = metadata.permissions();
            perms.set_mode(0o700);
            tokio::fs::set_permissions(dir, perms).await.ok();
        }
        tokio::fs::remove_dir_all(dir).await.ok();
    }
}
