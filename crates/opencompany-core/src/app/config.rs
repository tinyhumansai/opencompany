//! Precedence-resolved runtime configuration.
//!
//! [`RuntimeConfig`] is assembled from four layers, earlier winning over later:
//!
//! 1. Environment variables (`OPENCOMPANY_*`, `TINYHUMANS_*`, `GITHUB_TOKEN`).
//! 2. `~/.opencompany/config.toml`.
//! 3. The company manifest (`[brain].mode`, `[users].mode`).
//! 4. Built-in defaults.
//!
//! [`resolve`] returns the effective config together with a
//! [`ConfigProvenance`] recording *which* layer set each value, so
//! [`crate::app::doctor`] can explain the configuration back to the operator.
//! Resolution never touches the process environment directly: it reads through
//! the [`EnvSource`] seam, which tests satisfy with an in-memory map (no
//! `std::env::set_var` races).
//!
//! `api_url` defaults to the production TinyHumans hub, which is the right
//! built-in for a deployment an operator owns (self-hosted, desktop) — there
//! is nothing else it could mean. A [`Deployment::HostedTenant`] container
//! instead receives every setting from the platform that provisions it, so the
//! same silent default there is a platform bug wearing a working boot: the
//! tenant looks configured and talks to production regardless. `resolve`
//! refuses to fill it for a hosted tenant and fails loudly instead, naming the
//! variable that must be set.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

use crate::app::deployment::Deployment;
use crate::error::{OpenCompanyError, Result};
use crate::ports::types::SecretValue;

/// Default TinyHumans orchestration API base URL.
pub const DEFAULT_API_URL: &str = "https://api.tinyhumans.ai";

/// The variable that names the TinyHumans **site** — the dashboard an operator
/// is sent to for the two things the console deliberately cannot do: revoke a
/// key, and top the account up.
///
/// Almost never set. The site is derived from `api_url` by
/// [`site_for_api`](crate::server::hub_account::site_for_api), so a deployment
/// that points at staging moves both together; this exists for a front end the
/// convention does not describe.
pub const WEB_URL_ENV: &str = "TINYHUMANS_WEB_URL";

/// Default HTTP bind address for the local host.
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";

/// The config file name looked up under the data directory.
pub const CONFIG_FILE: &str = "config.toml";

// ---------------------------------------------------------------------------
// Brain mode
// ---------------------------------------------------------------------------

/// Which brain the runtime drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrainMode {
    /// Cognition is served by hosted Medulla over `/orchestration/v1`.
    Hosted,
    /// Cognition is served by a local sidecar process (a later phase).
    Sidecar,
}

impl BrainMode {
    /// The manifest/env spelling of this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hosted => "hosted",
            Self::Sidecar => "sidecar",
        }
    }
}

impl std::fmt::Display for BrainMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BrainMode {
    type Err = OpenCompanyError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "hosted" => Ok(Self::Hosted),
            "sidecar" => Ok(Self::Sidecar),
            other => Err(OpenCompanyError::Config(format!(
                "brain mode must be one of hosted, sidecar — you wrote `{other}`"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Auth mode
// ---------------------------------------------------------------------------

/// How humans prove who they are to a company.
///
/// One choice per company, made in configuration rather than in code, because
/// the three answers suit three different deployments and no host can serve all
/// three at once: offering a fallback would mean the weakest one is always
/// available, which is not a choice at all.
///
/// | Mode | Who signs in | How |
/// |---|---|---|
/// | [`Email`](Self::Email) | an invited address | magic link, optional password, ecosystem hub |
/// | [`Wallet`](Self::Wallet) | an invited base58 wallet | a signed challenge, no mailbox anywhere |
/// | [`None`](Self::None) | nobody | there is no sign-in; the app on this device *is* the owner |
///
/// [`Email`](Self::Email) is the default and is exactly the behaviour that
/// existed before this was configurable, so a company that names no mode is
/// unaffected.
///
/// [`None`](Self::None) is for the packaged desktop app, which binds loopback
/// and is used by the one person sitting at the machine. It does not merely skip
/// the login screen: the login routes are gone, and so is every route that would
/// add a second person, because a company nobody signs in to has no way to tell
/// one human from another and inviting someone would hand them an account they
/// could never reach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthMode {
    /// Magic-link (and optional password) sign-in over email. The default.
    #[default]
    Email,
    /// Sign-in by proving control of an Ed25519 wallet key.
    Wallet,
    /// No sign-in at all — a single implicit local owner. Desktop only.
    None,
}

impl AuthMode {
    /// The manifest/env spelling of this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Wallet => "wallet",
            Self::None => "none",
        }
    }

    /// Whether this mode has any sign-in flow at all.
    ///
    /// The inverse is the single question every login and user-administration
    /// route asks, so it is asked once, here, rather than by matching on the
    /// enum at each site and getting a later variant wrong.
    pub fn has_login(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this mode can address a mailbox — the gate on magic links,
    /// invite mail, and password login.
    pub fn uses_email(self) -> bool {
        matches!(self, Self::Email)
    }
}

impl std::fmt::Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AuthMode {
    type Err = OpenCompanyError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "email" => Ok(Self::Email),
            "wallet" => Ok(Self::Wallet),
            "none" => Ok(Self::None),
            other => Err(OpenCompanyError::Config(format!(
                "auth mode must be one of email, wallet, none — you wrote `{other}`"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

/// The layer that supplied a resolved value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigLayer {
    /// Set by an environment variable.
    Env,
    /// Set by `config.toml`.
    ConfigToml,
    /// Set by the company manifest.
    Manifest,
    /// Fell back to a built-in default.
    Default,
}

impl ConfigLayer {
    /// A short human label for doctor output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::ConfigToml => "config.toml",
            Self::Manifest => "manifest",
            Self::Default => "default",
        }
    }
}

/// Records which [`ConfigLayer`] set each effective config field.
#[derive(Clone, Debug, Default)]
pub struct ConfigProvenance(BTreeMap<&'static str, ConfigLayer>);

impl ConfigProvenance {
    /// Records that `field` was set by `layer`.
    fn set(&mut self, field: &'static str, layer: ConfigLayer) {
        self.0.insert(field, layer);
    }

    /// The layer that set `field`, if resolved.
    pub fn layer(&self, field: &str) -> Option<ConfigLayer> {
        self.0.get(field).copied()
    }

    /// Iterates `(field, layer)` pairs in stable field order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, ConfigLayer)> + '_ {
        self.0.iter().map(|(k, v)| (*k, *v))
    }
}

// ---------------------------------------------------------------------------
// Env seam
// ---------------------------------------------------------------------------

/// A read-only source of environment values. The `std::env`-backed
/// [`ProcessEnv`] is used at runtime; tests use a [`MapEnv`].
pub trait EnvSource {
    /// Returns the raw OS value for `key`, including empty and non-Unicode
    /// values. Configuration readers that must distinguish a malformed value
    /// from an unset one should use this rather than [`Self::get`].
    fn get_os(&self, key: &str) -> Option<std::ffi::OsString>;

    /// Returns the value for `key`, or `None` when unset or empty.
    fn get(&self, key: &str) -> Option<String> {
        self.get_os(key)
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty())
    }
}

/// Reads from the real process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get_os(&self, key: &str) -> Option<std::ffi::OsString> {
        std::env::var_os(key)
    }
}

/// An in-memory [`EnvSource`] for deterministic tests.
#[derive(Clone, Debug, Default)]
pub struct MapEnv(std::collections::HashMap<String, String>);

impl MapEnv {
    /// Builds a map env from `(key, value)` pairs.
    pub fn new<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl EnvSource for MapEnv {
    fn get_os(&self, key: &str) -> Option<std::ffi::OsString> {
        self.0.get(key).cloned().map(std::ffi::OsString::from)
    }
}

// ---------------------------------------------------------------------------
// config.toml mirror
// ---------------------------------------------------------------------------

/// A deserialized `~/.opencompany/config.toml`. Every field is optional so a
/// partial file only overrides what it names.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    /// TinyHumans API credential (the hosted-brain bearer token).
    pub tinyhumans_api_key: Option<String>,
    /// TinyHumans orchestration API base URL.
    pub api_url: Option<String>,
    /// TinyHumans site base URL, when the deployment's front end is not the one
    /// [`WEB_URL_ENV`] describes deriving. Unset — the normal case — derives it
    /// from `api_url`.
    pub web_url: Option<String>,
    /// Brain mode (`hosted` | `sidecar`).
    pub brain_mode: Option<String>,
    /// Auth mode (`email` | `wallet` | `none`), overriding every company's own
    /// `[users].mode` on this host. Absent — the normal case — leaves each
    /// company to name its own.
    pub auth_mode: Option<String>,
    /// HTTP bind address.
    pub bind: Option<String>,
    /// Data directory holding company bundles and this file.
    pub data_dir: Option<String>,
    /// OpenHuman sidecar base URL.
    pub openhuman_url: Option<String>,
    /// This host's public base URL — what it is reachable at from outside.
    pub public_url: Option<String>,
    /// GitHub token used by GitHub-backed tools.
    pub github_token: Option<String>,
    /// Unix-epoch milliseconds at which the first-run setup flow
    /// (`crate::server::setup`) was completed against this data root. Millis
    /// rather than a formatted date to match [`crate::ports::ids::now_millis`],
    /// which is how every other timestamp in this codebase is recorded.
    ///
    /// Absent means "never set up", which is what puts the console into the
    /// wizard instead of the sign-in form. It lives here rather than in browser
    /// storage on purpose: whether an *instance* has been configured is a fact
    /// about the instance, and keeping it in `localStorage` — the way the
    /// product tour keeps its own state — would re-run setup for every new
    /// browser and skip it for a data root restored onto a familiar one.
    pub setup_completed_at: Option<i64>,
    /// The `[workspace]` section: data-dir layout lifecycle knobs.
    pub workspace: WorkspaceSection,
    /// The `[memory]` section: which memory engine this instance binds, when
    /// the deployment has not named one through `OPENCOMPANY_MEMORY`
    /// (`docs/spec/runtime/memory-engine.md`).
    pub memory: MemorySection,
    /// `[[default_mcp_server]]` entries — MCP servers the packaged install
    /// registers and enables for every company, with no user setup (issue #527).
    ///
    /// This is the config location the issue asks for: changing what ships is an
    /// edit here, never a code change and never a per-company `company.toml`
    /// edit. Entries are normalized by
    /// [`normalize_default_servers`](crate::company::mcp::normalize_default_servers),
    /// which drops any that cannot ship and explains why.
    ///
    /// **An empty or absent list is authoritative** — it means "ship no
    /// defaults", never "fall back to a built-in set". There is deliberately no
    /// compiled-in list to fall back to.
    #[serde(rename = "default_mcp_server")]
    pub default_mcp_servers: Vec<crate::company::McpServer>,
}

/// The `[memory]` section of `config.toml`: the memory engine an operator
/// chose from the console, when the deployment did not inject one.
///
/// # Why this exists beside the env vars
///
/// The engine used to be selectable only through `OPENCOMPANY_MEMORY*`, which
/// means only by whoever controls the process environment. A self-hosted
/// operator who wants their company's memory in Supermemory or mem0 had to
/// edit a unit file and restart. This is the same second layer the rest of the
/// instance's configuration already has (`docs/spec/runtime/config.md`), so
/// the console can write the choice and
/// [`crate::server::ops::memory_engine`] can bind it live.
///
/// # Precedence, and why it is not "last writer wins"
///
/// `env ⟵ config.toml`, exactly as every other key resolves. A hosted tenant
/// has `OPENCOMPANY_MEMORY*` injected by the control plane, and a console that
/// accepted an edit there would write a file, report success, and change
/// nothing at the next boot — the silently-ignored-configuration failure the
/// setup flow refuses for the same reason. So when the env names an engine
/// this section is inert, the console renders read-only, and the write is
/// refused rather than accepted-and-dropped.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MemorySection {
    /// `store` | `embedded` | `remote` | `null`, parsed by
    /// [`MemoryBackend`](crate::store::MemoryBackend). Absent leaves the
    /// default (`store` — the base backend's own memory).
    pub backend: Option<String>,
    /// The engine id for a provider-backed mode: `supermemory`, `mem0`,
    /// `cognee`, or `namespace` for the in-pod contract store.
    pub driver: Option<String>,
    /// The hosted engine's endpoint.
    pub url: Option<String>,
    /// The hosted engine's credential.
    ///
    /// It lives in this file the same way `tinyhumans_api_key` and
    /// `github_token` already do — the file is the instance's private
    /// configuration, mode `0600` where the platform supports it — and it is
    /// never read back out over HTTP: the engine route reports whether a key
    /// is set, never its bytes.
    pub api_key: Option<String>,
}

/// The `[workspace]` section of `config.toml`: lifecycle of the canonical
/// data-dir layout (see [`DataLayout`](crate::store::DataLayout)).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WorkspaceSection {
    /// Turn each agent's private filesystem workspace into a Git repository and
    /// checkpoint changes after tool calls. Default: false.
    pub git_enabled: Option<bool>,
    /// Empty the ephemeral `tmp/` scratch directory on startup. Default: true.
    pub clear_tmp_on_startup: Option<bool>,
    /// Soft quota on the whole workspace, in gibibytes. Absent or `<= 0` means
    /// unlimited. Surfaced as an operator alert when exceeded; hard enforcement
    /// is the container/StorageClass layer's job (EFS access point / k8s
    /// `ResourceQuota`).
    pub storage_quota_gb: Option<f64>,
    /// Soft quota on the `tmp/` scratch directory, in gibibytes. Absent or
    /// `<= 0` means unlimited.
    pub tmp_quota_gb: Option<f64>,
    /// Hard quota on the total **binary payload** one company's workspace tree
    /// may hold, in gibibytes. Absent or `<= 0` means unlimited (issue #553).
    ///
    /// Unlike the two soft quotas above — which only warn, because hard
    /// enforcement of a whole data directory belongs to the container /
    /// StorageClass layer — this one is enforced at the store: a write that
    /// would cross it is refused before anything is stored. It can be, because
    /// the runtime knows the size of every payload it is asked to keep.
    pub tree_quota_gb: Option<f64>,
    /// Hard cap on a single workspace file, in mebibytes. Defaults to 256 MiB.
    ///
    /// Also the upload route's request body limit, so an over-cap upload is
    /// rejected at the edge rather than buffered and then refused.
    pub max_blob_mb: Option<f64>,
}

impl WorkspaceSection {
    /// Resolves the section against its defaults.
    pub fn resolve(&self) -> WorkspaceConfig {
        WorkspaceConfig {
            git_enabled: self.git_enabled.unwrap_or(false),
            clear_tmp_on_startup: self.clear_tmp_on_startup.unwrap_or(true),
            storage_quota_bytes: gib_to_bytes(self.storage_quota_gb),
            tmp_quota_bytes: gib_to_bytes(self.tmp_quota_gb),
            quota: crate::runtime::WorkspaceQuota {
                max_blob_bytes: self
                    .max_blob_mb
                    .filter(|m| *m > 0.0)
                    .map(|m| (m * 1024.0 * 1024.0) as u64)
                    .unwrap_or(crate::runtime::DEFAULT_MAX_BLOB_BYTES),
                tree_quota_bytes: gib_to_bytes(self.tree_quota_gb),
            },
        }
    }
}

/// Converts an optional gibibyte quota to bytes, treating absent / non-positive
/// values as "unlimited" (`None`).
fn gib_to_bytes(gb: Option<f64>) -> Option<u64> {
    gb.filter(|g| *g > 0.0)
        .map(|g| (g * 1024.0 * 1024.0 * 1024.0) as u64)
}

/// Resolved `[workspace]` configuration.
#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    /// Whether private agent workspaces keep automatic Git checkpoints.
    pub git_enabled: bool,
    /// Whether the ephemeral `tmp/` scratch is cleared on startup.
    pub clear_tmp_on_startup: bool,
    /// Soft whole-workspace quota in bytes; `None` is unlimited.
    pub storage_quota_bytes: Option<u64>,
    /// Soft `tmp/` quota in bytes; `None` is unlimited.
    pub tmp_quota_bytes: Option<u64>,
    /// The workspace tree's **enforced** byte limits (issue #553).
    pub quota: crate::runtime::WorkspaceQuota,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            git_enabled: false,
            clear_tmp_on_startup: true,
            storage_quota_bytes: None,
            tmp_quota_bytes: None,
            quota: crate::runtime::WorkspaceQuota::default(),
        }
    }
}

impl ConfigFile {
    /// Loads `config.toml` from `dir`, returning `None` when the file is
    /// absent. A malformed file is a hard [`OpenCompanyError::Config`] error.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(OpenCompanyError::Config(format!(
                    "could not read {}: {e}",
                    path.display()
                )));
            }
        };
        let parsed = toml::from_str(&text).map_err(|e| {
            OpenCompanyError::Config(format!("{} is not valid TOML: {}", path.display(), e))
        })?;
        Ok(Some(parsed))
    }
}

// ---------------------------------------------------------------------------
// config.toml writer
// ---------------------------------------------------------------------------

/// A value the setup flow writes into `config.toml`.
///
/// [`Unset`](ConfigValue::Unset) removes the key rather than writing an empty
/// string, because the two are not the same to [`resolve`]: an absent key falls
/// through to the next layer, while `""` is read by [`EnvSource`]-shaped logic
/// as a set-but-blank value. "Clear this and let the default apply" has to mean
/// deletion.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    /// A string value (`bind`, `auth_mode`, `api_url`, …).
    Str(String),
    /// A boolean (`workspace.clear_tmp_on_startup`).
    Bool(bool),
    /// A number (the `[workspace]` quotas, all of which are floats).
    Float(f64),
    /// An integer (`setup_completed_at`, in epoch millis).
    Int(i64),
    /// Remove the key entirely, letting the next precedence layer supply it.
    Unset,
}

/// Applies `edits` to the `config.toml` under `dir`, creating the file when it
/// does not exist, and returns the path written.
///
/// Each key is dotted: `"bind"` is a top-level key, `"workspace.max_blob_mb"` a
/// key inside the `[workspace]` table. Only the named keys are touched —
/// **every other key, every comment, and the existing key order survive**,
/// which is the whole reason this goes through `toml_edit` rather than
/// serializing a [`ConfigFile`]. The shipped file's commented
/// `[[default_mcp_server]]` PLACEHOLDER block is documentation an operator is
/// meant to read and uncomment (`docs/spec/runtime/config.md`), and a
/// round-trip through the struct would silently delete it.
///
/// Serializes every [`write_config_toml`] call in this process. See that
/// function's doc for why a single process-wide lock, rather than one keyed
/// per directory, is the right shape here.
static CONFIG_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A temp-file name no two calls in this process can collide on, and that two
/// processes racing the same directory are very unlikely to either.
fn unique_tmp_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{CONFIG_FILE}.{}.{nanos}.{call}.tmp", std::process::id())
}

/// The write is atomic: the document is rendered to a uniquely-named temporary
/// file in the same directory and then `rename`d over the target, so a crash
/// mid-write cannot leave a half-written config that the next boot refuses to
/// parse. Same-directory because `rename` is only atomic within one
/// filesystem.
///
/// A malformed existing file is a hard error, matching [`ConfigFile::load`]:
/// merging into a document that could not be parsed would mean overwriting
/// whatever the operator actually had there.
///
/// The whole read-edit-write-rename sequence runs under [`CONFIG_WRITE_LOCK`],
/// so two concurrent setup requests cannot each parse the same on-disk
/// snapshot and then race to replace it — the second call's edits would
/// otherwise silently overwrite the first's rather than merging with them.
/// The lock is process-wide rather than per-directory: this process serves at
/// most a handful of config roots, and a coarser lock that is trivially
/// correct beats a per-path one that has to get eviction right. The temporary
/// file's name is still made unique per call (process id, a monotonic call
/// counter, and the current time), independent of the lock: it is what keeps
/// two *processes* pointed at the same directory (a misconfiguration, but one
/// this should not corrupt) from ever writing through the same temp path.
pub fn write_config_toml(dir: &Path, edits: &[(&str, ConfigValue)]) -> Result<PathBuf> {
    use toml_edit::{DocumentMut, Item, Table, value};

    let _guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let path = dir.join(CONFIG_FILE);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(OpenCompanyError::Config(format!(
                "could not read {}: {e}",
                path.display()
            )));
        }
    };
    let mut doc: DocumentMut = existing.parse().map_err(|e| {
        OpenCompanyError::Config(format!("{} is not valid TOML: {}", path.display(), e))
    })?;

    for (key, new_value) in edits {
        // At most one level of nesting: `workspace.max_blob_mb`. Nothing the
        // setup flow writes goes deeper, and `[[default_mcp_server]]` is an
        // array of tables that stays hand-edited by design.
        let (table_name, leaf) = match key.split_once('.') {
            Some((table, leaf)) => (Some(table), leaf),
            None => (None, *key),
        };

        let target: &mut Table = match table_name {
            None => doc.as_table_mut(),
            Some(name) => {
                if matches!(new_value, ConfigValue::Unset) && !doc.contains_key(name) {
                    // Nothing to clear, and materializing an empty `[workspace]`
                    // table to delete a key out of it would add noise to the file.
                    continue;
                }
                let entry = doc
                    .entry(name)
                    .or_insert_with(|| Item::Table(Table::new()))
                    .as_table_mut()
                    .ok_or_else(|| {
                        OpenCompanyError::Config(format!(
                            "{} has a `{name}` entry that is not a table",
                            path.display()
                        ))
                    })?;
                // An implicit table renders as bare `key = ...` lines with no
                // `[workspace]` header, which parses back differently.
                entry.set_implicit(false);
                entry
            }
        };

        match new_value {
            ConfigValue::Str(v) => target[leaf] = value(v.as_str()),
            ConfigValue::Bool(v) => target[leaf] = value(*v),
            ConfigValue::Float(v) => target[leaf] = value(*v),
            ConfigValue::Int(v) => target[leaf] = value(*v),
            ConfigValue::Unset => {
                target.remove(leaf);
            }
        }
    }

    let tmp = dir.join(unique_tmp_name());
    std::fs::write(&tmp, doc.to_string()).map_err(|e| {
        // `write` can fail after partially creating the file (for example, a
        // write that fails mid-way rather than at open). Clear it so a
        // failed apply never leaves a stray file for the next boot, or the
        // next write, to trip over. Best-effort: if this also fails there is
        // nothing more to do, and the original write error is what matters.
        let _ = std::fs::remove_file(&tmp);
        OpenCompanyError::Config(format!("could not write {}: {e}", tmp.display()))
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        // The write above succeeded, so the temp file exists; `rename` can
        // still fail (for example if `path` is replaced by a directory).
        let _ = std::fs::remove_file(&tmp);
        OpenCompanyError::Config(format!(
            "could not replace {} with {}: {e}",
            path.display(),
            tmp.display()
        ))
    })?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Resolved config
// ---------------------------------------------------------------------------

/// The effective runtime configuration after precedence resolution.
#[derive(Clone)]
pub struct RuntimeConfig {
    /// HTTP bind address for the local host.
    pub bind: String,
    /// Data directory holding company bundles and `config.toml`.
    pub data_dir: PathBuf,
    /// TinyHumans orchestration API base URL.
    pub api_url: String,
    /// TinyHumans site base URL, when one is stated. `None` derives it from
    /// [`Self::api_url`] — see [`WEB_URL_ENV`].
    pub web_url: Option<String>,
    /// Which brain the runtime drives.
    pub brain_mode: BrainMode,
    /// How humans sign in to this company.
    pub auth_mode: AuthMode,
    /// OpenHuman sidecar base URL, if configured.
    pub openhuman_url: Option<String>,
    /// This host's public base URL — what it is reachable at from outside —
    /// if configured.
    pub public_url: Option<String>,
    /// GitHub token, if configured. Redacted in `Debug`.
    pub github_token: Option<SecretValue>,
    /// TinyHumans hosted-brain credential, if configured. Redacted in `Debug`.
    pub tinyhumans_credential: Option<SecretValue>,
    /// Path to the platform-projected TinyHumans token file
    /// ([`TOKEN_FILE_ENV`](crate::company::credentials::TOKEN_FILE_ENV)), when the
    /// platform hands this instance a rotating, audience-bound identity instead of
    /// a static key. A path, not a secret — safe to print.
    pub tinyhumans_token_file: Option<PathBuf>,
    /// Resolved `[workspace]` data-dir layout configuration.
    pub workspace: WorkspaceConfig,
    /// Install-wide default MCP servers, already normalized (issue #527).
    /// Empty when the install configures none, which is the common case and
    /// leaves MCP resolution byte-identical to the manifest/runtime pair.
    pub default_mcp_servers: Vec<crate::company::McpServer>,
}

impl RuntimeConfig {
    /// True when hosted cognition can run: hosted mode plus a credential this
    /// instance can **obtain** — see [`Self::credential_available`].
    pub fn cycles_available(&self) -> bool {
        self.brain_mode == BrainMode::Hosted && self.credential_available()
    }

    /// Whether a TinyHumans credential can be obtained at all.
    ///
    /// The question is "can I get a token?", not "do I hold a secret?": a hosted
    /// tenant holds nothing and reads a projected file that rotates in place, so
    /// asking about a stored secret would report a perfectly healthy instance as
    /// unable to think.
    pub fn credential_available(&self) -> bool {
        self.credential_source() != crate::company::CredentialSource::None
    }

    /// Which tier the credential comes from, for operator-facing output.
    ///
    /// Delegates to
    /// [`TinyhumansTokenSource::source_of_parts`](crate::company::credentials::TinyhumansTokenSource::source_of_parts)
    /// rather than restating the rule: the projected tier counts only when the
    /// named path **exists**, so a leftover `TINYHUMANS_TOKEN_FILE` pointing at
    /// something the runtime never mounted reports the static tier (or `none`)
    /// instead of claiming an identity this instance cannot present.
    pub fn credential_source(&self) -> crate::company::CredentialSource {
        crate::company::credentials::TinyhumansTokenSource::source_of_parts(
            self.tinyhumans_token_file.as_deref(),
            self.tinyhumans_credential.is_some(),
        )
    }
}

/// A manual `Debug` that redacts both secret handles so a credential can never
/// reach a log line or panic message.
impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field("bind", &self.bind)
            .field("data_dir", &self.data_dir)
            .field("api_url", &self.api_url)
            .field("brain_mode", &self.brain_mode)
            .field("auth_mode", &self.auth_mode)
            .field("openhuman_url", &self.openhuman_url)
            .field("public_url", &self.public_url)
            .field("github_token", &redacted(&self.github_token))
            .field(
                "tinyhumans_credential",
                &redacted(&self.tinyhumans_credential),
            )
            .field("tinyhumans_token_file", &self.tinyhumans_token_file)
            .finish()
    }
}

/// Renders a secret handle as `set`/`missing`, never its bytes.
pub(crate) fn redacted(value: &Option<SecretValue>) -> &'static str {
    if value.is_some() { "set" } else { "missing" }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolves the effective [`RuntimeConfig`] and its [`ConfigProvenance`].
///
/// `env` supplies environment values, `config_toml` an optional parsed
/// `config.toml`, and `manifest` the company manifest whose `[brain].mode`
/// participates in `brain_mode` resolution.
pub fn resolve(
    env: &dyn EnvSource,
    config_toml: Option<&ConfigFile>,
    manifest: &crate::company::CompanyManifest,
) -> Result<(RuntimeConfig, ConfigProvenance)> {
    let mut prov = ConfigProvenance::default();
    let deployment = Deployment::from_env(env);

    let bind = resolve_str(
        &mut prov,
        "bind",
        env.get("OPENCOMPANY_BIND"),
        config_toml.and_then(|c| c.bind.clone()),
        None,
        DEFAULT_BIND.to_string(),
    );

    let data_dir = resolve_str(
        &mut prov,
        "data_dir",
        env.get("OPENCOMPANY_DATA_DIR"),
        config_toml.and_then(|c| c.data_dir.clone()),
        None,
        default_data_dir_str(env),
    );

    let api_url = resolve_base_url(
        &mut prov,
        "api_url",
        "TINYHUMANS_API_URL",
        deployment,
        HostedDefault::Refuse,
        BaseUrlSources {
            env: env.get("TINYHUMANS_API_URL"),
            toml: config_toml.and_then(|c| c.api_url.clone()),
            default: DEFAULT_API_URL.to_string(),
        },
    )?;

    // No `HostedDefault::Refuse` twin: unset is not a silent production default
    // here, it is "derive from whichever hub this deployment already named".
    //
    // Each candidate is trimmed and blanked out *before* `resolve_opt` picks
    // between them — trimming only the winner would let a whitespace-only env
    // value outrank a real TOML one instead of falling through to it.
    let web_url = resolve_opt(
        &mut prov,
        "web_url",
        env.get(WEB_URL_ENV)
            .filter(|value| !value.trim().is_empty()),
        config_toml
            .and_then(|c| c.web_url.clone())
            .filter(|value| !value.trim().is_empty()),
    );

    // brain_mode: env <- config.toml <- manifest (always present) <- default.
    let brain_raw = resolve_str(
        &mut prov,
        "brain_mode",
        env.get("OPENCOMPANY_BRAIN_MODE"),
        config_toml.and_then(|c| c.brain_mode.clone()),
        Some(manifest.brain.mode.clone()),
        BrainMode::Hosted.as_str().to_string(),
    );
    let brain_mode = BrainMode::from_str(&brain_raw)?;

    // auth_mode: env <- config.toml <- manifest (always present) <- default.
    //
    // Unlike brain_mode this resolution is not the last word, because `serve`
    // hosts N companies and this pass sees one manifest. The env and config.toml
    // layers are host-wide and are carried to every company as
    // `AppConfig::auth_mode_override`; the manifest layer is per company and is
    // read from that company's own `[users].mode` when its runtime is built. The
    // precedence is the same either way — see
    // [`RuntimeBuilder::with_auth_mode_override`](crate::runtime::RuntimeBuilder::with_auth_mode_override).
    let auth_raw = resolve_str(
        &mut prov,
        "auth_mode",
        env.get("OPENCOMPANY_AUTH_MODE"),
        config_toml.and_then(|c| c.auth_mode.clone()),
        Some(manifest.users.mode.clone()),
        AuthMode::default().as_str().to_string(),
    );
    let auth_mode = AuthMode::from_str(&auth_raw)?;

    let openhuman_url = resolve_opt(
        &mut prov,
        "openhuman_url",
        env.get("OPENCOMPANY_OPENHUMAN_URL"),
        config_toml.and_then(|c| c.openhuman_url.clone()),
    );

    let public_url = resolve_opt(
        &mut prov,
        "public_url",
        env.get("OPENCOMPANY_PUBLIC_URL"),
        config_toml.and_then(|c| c.public_url.clone()),
    );

    let github_token = resolve_opt(
        &mut prov,
        "github_token",
        env.get("GITHUB_TOKEN"),
        config_toml.and_then(|c| c.github_token.clone()),
    )
    .map(SecretValue);

    let tinyhumans_credential = resolve_opt(
        &mut prov,
        "tinyhumans_credential",
        env.get(crate::company::credentials::API_KEY_ENV),
        config_toml.and_then(|c| c.tinyhumans_api_key.clone()),
    )
    .map(SecretValue);

    // The projected token file is injected by the platform, never written by an
    // operator, so it has no `config.toml` layer to fall back to.
    let tinyhumans_token_file = resolve_opt(
        &mut prov,
        "tinyhumans_token_file",
        env.get(crate::company::credentials::TOKEN_FILE_ENV),
        None,
    )
    .map(PathBuf::from);

    let workspace = config_toml
        .map(|c| c.workspace.resolve())
        .unwrap_or_default();

    // Install-wide MCP defaults (issue #527). Normalized here, once, at the
    // config boundary rather than at each read: a rejected entry is an operator
    // mistake in a packaged file, and it should be named at boot — where
    // somebody is looking — instead of silently thinning the list on every
    // company's first agent turn.
    //
    // A rejection is a warning, not a boot failure. These servers are additive
    // convenience; refusing to start an install because one shipped default has
    // a bad URL would turn a cosmetic packaging error into an outage.
    let default_mcp_servers = match config_toml {
        Some(c) if !c.default_mcp_servers.is_empty() => {
            let (kept, problems) =
                crate::company::mcp::normalize_default_servers(&c.default_mcp_servers);
            for problem in &problems {
                tracing::warn!(target: "opencompany::config", "{problem}");
            }
            kept
        }
        _ => Vec::new(),
    };

    let config = RuntimeConfig {
        bind,
        data_dir: PathBuf::from(data_dir),
        api_url,
        web_url,
        brain_mode,
        auth_mode,
        openhuman_url,
        public_url,
        github_token,
        tinyhumans_credential,
        tinyhumans_token_file,
        workspace,
        default_mcp_servers,
    };
    Ok((config, prov))
}

/// Resolves the address `serve` binds its HTTP listener to, with the layer
/// that supplied it.
///
/// Precedence: `--bind` flag ⟵ `OPENCOMPANY_BIND` ⟵ `config.toml` `bind` ⟵
/// [`DEFAULT_BIND`]. This mirrors the [`resolve`] chain for every other field,
/// but stands apart because it takes a CLI flag as its top layer and no company
/// manifest: `serve` hosts N companies, so there is no single manifest to feed
/// the full [`resolve`] pass.
///
/// The returned label is operator-facing (`"--bind"` / `"OPENCOMPANY_BIND"` /
/// `"config.toml"` / `"default"`), so startup can print *which* layer chose the
/// address and a mismatch is visible rather than silent.
///
/// An empty `OPENCOMPANY_BIND` counts as unset — the [`EnvSource`] contract —
/// and falls through to the next layer. An empty flag or `config.toml` value is
/// taken verbatim (as in [`resolve_str`]) and fails loudly at bind time rather
/// than silently reverting to the default.
///
/// The default stays loopback. A wildcard bind is only ever reached by an
/// explicit flag, variable, or config entry — i.e. by operator intent.
pub fn resolve_serve_bind(
    flag: Option<String>,
    env: &dyn EnvSource,
    config_bind: Option<String>,
) -> (String, &'static str) {
    if let Some(value) = flag {
        (value, "--bind")
    } else if let Some(value) = env.get("OPENCOMPANY_BIND") {
        (value, "OPENCOMPANY_BIND")
    } else if let Some(value) = config_bind {
        (value, "config.toml")
    } else {
        (DEFAULT_BIND.to_string(), "default")
    }
}

/// Resolves a required string field, recording its winning layer.
fn resolve_str(
    prov: &mut ConfigProvenance,
    field: &'static str,
    env_val: Option<String>,
    toml_val: Option<String>,
    manifest_val: Option<String>,
    default_val: String,
) -> String {
    if let Some(value) = env_val {
        prov.set(field, ConfigLayer::Env);
        value
    } else if let Some(value) = toml_val {
        prov.set(field, ConfigLayer::ConfigToml);
        value
    } else if let Some(value) = manifest_val {
        prov.set(field, ConfigLayer::Manifest);
        value
    } else {
        prov.set(field, ConfigLayer::Default);
        default_val
    }
}

/// Whether a [`Deployment::HostedTenant`] must state a base URL rather than
/// inherit the built-in default.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HostedDefault {
    /// Refuse to boot when nothing states it. For a backend the tenant talks
    /// to unconditionally, a silent production default is a destination
    /// nobody chose.
    Refuse,
    /// Fall back to the default like any other deployment. For a backend
    /// reached only when the company opts in, where the consumer carries its
    /// own fallback anyway.
    Allow,
}

/// Resolves a base-URL field that names which real backend this process talks
/// to (`api_url`).
///
/// Behaves exactly like [`resolve_str`] for [`Deployment::SelfHosted`] and
/// [`Deployment::Desktop`]: env, then `config.toml`, then the built-in
/// default — the operator running either owns the choice, and the default
/// (production) *is* that choice when they name nothing.
///
/// For [`Deployment::HostedTenant`] the default is refused when `hosted` is
/// [`HostedDefault::Refuse`]: a tenant container is handed its entire
/// environment by the platform that provisions it, so nobody ever chose the
/// destination that a silent default would apply. The caller gets a config
/// error naming `var_name` rather than a container that boots and quietly
/// talks to production.
///
/// That argument holds only for a backend every tenant reaches. It does not
/// hold for one gated behind an opt-in the tenant has not taken: refusing
/// there stops a container over a URL it would never have built a client for.
/// Such a field passes [`HostedDefault::Allow`] and keeps defaulting.
///
/// An env or `config.toml` value that is empty (after trimming) counts as
/// unset in both branches — a launcher that exported the variable with
/// nothing in it has said nothing.
/// Where a base URL may come from, in precedence order.
pub(crate) struct BaseUrlSources {
    /// The process environment.
    pub(crate) env: Option<String>,
    /// `config.toml`.
    pub(crate) toml: Option<String>,
    /// The built-in default, which for every field here is production.
    pub(crate) default: String,
}

pub(crate) fn resolve_base_url(
    prov: &mut ConfigProvenance,
    field: &'static str,
    var_name: &str,
    deployment: Deployment,
    hosted: HostedDefault,
    sources: BaseUrlSources,
) -> Result<String> {
    let BaseUrlSources {
        env: env_val,
        toml: toml_val,
        default: default_val,
    } = sources;
    if let Some(value) = env_val.filter(|v| !v.trim().is_empty()) {
        prov.set(field, ConfigLayer::Env);
        return Ok(value);
    }
    if let Some(value) = toml_val.filter(|v| !v.trim().is_empty()) {
        prov.set(field, ConfigLayer::ConfigToml);
        return Ok(value);
    }
    if deployment == Deployment::HostedTenant && hosted == HostedDefault::Refuse {
        return Err(OpenCompanyError::Config(format!(
            "{var_name} is not set. This is a hosted-tenant deployment, which is handed its \
             whole environment by the platform that provisions it — so this refuses to boot \
             rather than silently default to production. Set {var_name} explicitly (the \
             production hub, or the staging hub for a staging tenant)."
        )));
    }
    prov.set(field, ConfigLayer::Default);
    Ok(default_val)
}

/// Resolves an optional string field, recording its winning layer (`Default`
/// when unset by every source).
pub(crate) fn resolve_opt(
    prov: &mut ConfigProvenance,
    field: &'static str,
    env_val: Option<String>,
    toml_val: Option<String>,
) -> Option<String> {
    if let Some(value) = env_val {
        prov.set(field, ConfigLayer::Env);
        Some(value)
    } else if let Some(value) = toml_val {
        prov.set(field, ConfigLayer::ConfigToml);
        Some(value)
    } else {
        prov.set(field, ConfigLayer::Default);
        None
    }
}

/// The default data directory: `$HOME/.opencompany`, falling back to a relative
/// path when `$HOME` is unset.
fn default_data_dir_str(env: &dyn EnvSource) -> String {
    match env.get("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".opencompany")
            .to_string_lossy()
            .into_owned(),
        None => PathBuf::from(".opencompany").to_string_lossy().into_owned(),
    }
}

/// The data directory read straight off the process environment
/// (`OPENCOMPANY_DATA_DIR`, else `$HOME/.opencompany`) — the per-instance
/// workspace root. For callers like `serve` and `doctor` that resolve the data
/// root before (or without) the full [`resolve`] precedence pass.
pub fn data_dir_from_env() -> PathBuf {
    data_dir_from_source(&ProcessEnv)
}

/// Resolves the instance data directory from an injected environment source.
pub fn data_dir_from_source(env: &dyn EnvSource) -> PathBuf {
    data_dir_from(
        env.get_os("OPENCOMPANY_DATA_DIR"),
        env.get_os("HOME"),
        env.get_os("USERPROFILE"),
    )
}

/// Pure core of [`data_dir_from_env`]: resolves the data dir from the raw
/// `OPENCOMPANY_DATA_DIR` and `HOME` values. Empty strings are treated as unset
/// — an empty `OPENCOMPANY_DATA_DIR` would otherwise resolve to the process
/// working directory rather than falling back to `$HOME/.opencompany`.
fn data_dir_from(
    data_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    // Windows sets this rather than `HOME`. Without it the fallback below is a
    // RELATIVE path resolved against the working directory — see
    // `store::paths::resolve_home_from`, which has the same branch for the same
    // reason. The two must agree, or a Windows host would put its bundles and
    // its workspace in different places.
    user_profile: Option<std::ffi::OsString>,
) -> PathBuf {
    let non_empty = |v: Option<std::ffi::OsString>| v.filter(|value| !value.is_empty());
    match non_empty(data_dir) {
        Some(dir) => PathBuf::from(dir),
        None => match non_empty(home).or_else(|| non_empty(user_profile)) {
            Some(home) => PathBuf::from(home).join(".opencompany"),
            None => PathBuf::from(".opencompany"),
        },
    }
}

#[cfg(test)]
#[path = "config_resolution_tests.rs"]
mod config_resolution_tests;
#[cfg(test)]
#[path = "config_serialization_tests.rs"]
mod config_serialization_tests;
