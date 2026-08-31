//! Precedence-resolved runtime configuration.
//!
//! [`RuntimeConfig`] is assembled from four layers, earlier winning over later:
//!
//! 1. Environment variables (`OPENCOMPANY_*`, `TINYHUMANS_*`, `TINYPLACE_*`,
//!    `GITHUB_TOKEN`).
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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

use crate::error::{OpenCompanyError, Result};
use crate::ports::types::SecretValue;

/// Default TinyHumans orchestration API base URL.
pub const DEFAULT_API_URL: &str = "https://api.tinyhumans.ai";

/// Default tiny.place economy API base URL.
pub const DEFAULT_TINYPLACE_API_URL: &str = "https://api.tiny.place";

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
    /// tiny.place economy API base URL.
    pub tinyplace_api_url: Option<String>,
    /// Public host base URL advertised in published Agent Cards.
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
    /// Which brain the runtime drives.
    pub brain_mode: BrainMode,
    /// How humans sign in to this company.
    pub auth_mode: AuthMode,
    /// OpenHuman sidecar base URL, if configured.
    pub openhuman_url: Option<String>,
    /// tiny.place economy API base URL.
    pub tinyplace_api_url: String,
    /// Public host base URL advertised in published Agent Cards, if configured.
    /// When unset, the card endpoint falls back to `http://{bind}`.
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
            .field("tinyplace_api_url", &self.tinyplace_api_url)
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

    let api_url = resolve_str(
        &mut prov,
        "api_url",
        env.get("TINYHUMANS_API_URL"),
        config_toml.and_then(|c| c.api_url.clone()),
        None,
        DEFAULT_API_URL.to_string(),
    );

    let tinyplace_api_url = resolve_str(
        &mut prov,
        "tinyplace_api_url",
        env.get("TINYPLACE_API_URL"),
        config_toml.and_then(|c| c.tinyplace_api_url.clone()),
        None,
        DEFAULT_TINYPLACE_API_URL.to_string(),
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
        brain_mode,
        auth_mode,
        openhuman_url,
        tinyplace_api_url,
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

/// Resolves an optional string field, recording its winning layer (`Default`
/// when unset by every source).
fn resolve_opt(
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
mod test {
    use super::*;
    use crate::company::CompanyManifest;

    fn manifest_with_brain(mode: &str) -> CompanyManifest {
        let toml_src = format!("[company]\nname = \"X\"\n[brain]\nmode = \"{mode}\"\n");
        toml::from_str(&toml_src).expect("valid manifest")
    }

    fn default_manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"X\"\n").expect("valid manifest")
    }

    #[test]
    fn defaults_fill_in_when_nothing_set() {
        let env = MapEnv::default();
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();

        assert_eq!(cfg.api_url, DEFAULT_API_URL);
        assert_eq!(cfg.tinyplace_api_url, DEFAULT_TINYPLACE_API_URL);
        assert_eq!(cfg.bind, DEFAULT_BIND);
        assert_eq!(cfg.brain_mode, BrainMode::Hosted);
        assert!(cfg.tinyhumans_credential.is_none());
        assert!(cfg.github_token.is_none());
        assert!(!cfg.cycles_available());

        // The manifest always supplies a brain mode, so its layer is Manifest.
        assert_eq!(prov.layer("brain_mode"), Some(ConfigLayer::Manifest));
        assert_eq!(prov.layer("api_url"), Some(ConfigLayer::Default));
        assert_eq!(prov.layer("bind"), Some(ConfigLayer::Default));
    }

    #[test]
    fn data_dir_from_treats_empty_as_unset() {
        use std::ffi::OsString;
        // An empty OPENCOMPANY_DATA_DIR falls back to $HOME/.opencompany, not cwd.
        assert_eq!(
            data_dir_from(
                Some(OsString::from("")),
                Some(OsString::from("/home/u")),
                None
            ),
            PathBuf::from("/home/u/.opencompany")
        );
        // A set value is used verbatim.
        assert_eq!(
            data_dir_from(
                Some(OsString::from("/data")),
                Some(OsString::from("/home/u")),
                None
            ),
            PathBuf::from("/data")
        );
        // Neither set → the relative default.
        assert_eq!(
            data_dir_from(None, None, None),
            PathBuf::from(".opencompany")
        );
        // Windows: `USERPROFILE` stands in, so the data dir does not become a
        // relative path resolved against the working directory. Must agree with
        // `store::paths::resolve_home_from`, or a Windows host would split its
        // bundles from its workspace.
        assert_eq!(
            data_dir_from(None, None, Some(OsString::from("C:\\Users\\ada"))),
            PathBuf::from("C:\\Users\\ada").join(".opencompany")
        );
    }

    #[test]
    fn resolve_propagates_workspace_to_runtime_config() {
        let env = MapEnv::default();
        let file = ConfigFile {
            workspace: WorkspaceSection {
                git_enabled: Some(true),
                clear_tmp_on_startup: Some(false),
                ..WorkspaceSection::default()
            },
            ..ConfigFile::default()
        };
        let (cfg, _) = resolve(&env, Some(&file), &default_manifest()).unwrap();
        assert!(!cfg.workspace.clear_tmp_on_startup);
        assert!(cfg.workspace.git_enabled);

        // An absent `[workspace]` section resolves to the default (clear on boot).
        let (cfg, _) = resolve(&env, None, &default_manifest()).unwrap();
        assert!(cfg.workspace.clear_tmp_on_startup);
        assert!(!cfg.workspace.git_enabled);
    }

    #[test]
    fn default_mcp_servers_resolve_from_config_toml_and_are_normalized() {
        // Issue #527: the config layer is the whole "no code change" claim, so
        // it is asserted rather than trusted — a list that silently failed to
        // resolve looks identical to one nobody configured.
        fn entry(name: &str, endpoint: &str) -> crate::company::McpServer {
            crate::company::McpServer {
                name: name.to_string(),
                endpoint: endpoint.to_string(),
                ..Default::default()
            }
        }
        let env = MapEnv::default();

        // A clean entry reaches RuntimeConfig.
        let file = ConfigFile {
            default_mcp_servers: vec![entry("deepwiki", "https://deepwiki.example/mcp")],
            ..ConfigFile::default()
        };
        let (cfg, _) = resolve(&env, Some(&file), &default_manifest()).unwrap();
        assert_eq!(cfg.default_mcp_servers.len(), 1);
        assert_eq!(cfg.default_mcp_servers[0].name, "deepwiki");

        // An unshippable entry is dropped here, at the boundary, rather than
        // thinning the list on every company's first agent turn — and it does
        // not take the good one with it, nor fail the boot.
        let file = ConfigFile {
            default_mcp_servers: vec![
                entry("leaky", "https://api.example/mcp?apiKey=leaked"),
                entry("clean", "https://clean.example/mcp"),
            ],
            ..ConfigFile::default()
        };
        let (cfg, _) = resolve(&env, Some(&file), &default_manifest()).unwrap();
        let names: Vec<&str> = cfg
            .default_mcp_servers
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["clean"]);

        // Absent section => no defaults, and emphatically not a built-in list.
        let (cfg, _) = resolve(&env, None, &default_manifest()).unwrap();
        assert!(cfg.default_mcp_servers.is_empty());
    }

    #[test]
    fn default_mcp_servers_parse_from_the_toml_array_of_tables() {
        // Pins the wire name operators actually type. A rename would compile
        // fine and silently stop reading their config.
        let file: ConfigFile = toml::from_str(
            r#"
            [[default_mcp_server]]
            name = "deepwiki"
            endpoint = "https://mcp.deepwiki.com/mcp"
            description = "Docs for public repos."
            "#,
        )
        .expect("parses");
        assert_eq!(file.default_mcp_servers.len(), 1);
        assert_eq!(file.default_mcp_servers[0].name, "deepwiki");
        assert_eq!(
            file.default_mcp_servers[0].endpoint,
            "https://mcp.deepwiki.com/mcp"
        );
    }

    #[test]
    fn env_beats_config_toml_beats_manifest_beats_default() {
        // brain_mode: env wins over everything.
        let env = MapEnv::new([
            ("OPENCOMPANY_BRAIN_MODE", "sidecar"),
            ("OPENCOMPANY_BIND", "0.0.0.0:9000"),
        ]);
        let file = ConfigFile {
            brain_mode: Some("hosted".into()),
            bind: Some("127.0.0.1:1111".into()),
            api_url: Some("https://toml.example".into()),
            ..ConfigFile::default()
        };
        let (cfg, prov) = resolve(&env, Some(&file), &manifest_with_brain("hosted")).unwrap();

        assert_eq!(cfg.brain_mode, BrainMode::Sidecar);
        assert_eq!(prov.layer("brain_mode"), Some(ConfigLayer::Env));
        assert_eq!(cfg.bind, "0.0.0.0:9000");
        assert_eq!(prov.layer("bind"), Some(ConfigLayer::Env));

        // api_url only in config.toml, so config.toml wins over the default.
        assert_eq!(cfg.api_url, "https://toml.example");
        assert_eq!(prov.layer("api_url"), Some(ConfigLayer::ConfigToml));
    }

    fn manifest_with_auth(mode: &str) -> CompanyManifest {
        let toml_src = format!("[company]\nname = \"X\"\n[users]\nmode = \"{mode}\"\n");
        toml::from_str(&toml_src).expect("valid manifest")
    }

    /// A manifest naming no mode signs people in by email — which is what every
    /// company did before the mode existed, so no deployment changes behaviour
    /// by upgrading.
    #[test]
    fn auth_mode_defaults_to_email() {
        let env = MapEnv::default();
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        assert_eq!(cfg.auth_mode, AuthMode::Email);
        // The manifest always supplies one (serde fills the default), exactly
        // as it does for the brain mode.
        assert_eq!(prov.layer("auth_mode"), Some(ConfigLayer::Manifest));
    }

    #[test]
    fn manifest_supplies_auth_mode_when_env_and_toml_absent() {
        let env = MapEnv::default();
        let (cfg, prov) = resolve(&env, None, &manifest_with_auth("wallet")).unwrap();
        assert_eq!(cfg.auth_mode, AuthMode::Wallet);
        assert_eq!(prov.layer("auth_mode"), Some(ConfigLayer::Manifest));
    }

    #[test]
    fn config_toml_beats_the_manifest_for_auth_mode() {
        let env = MapEnv::default();
        let file = ConfigFile {
            auth_mode: Some("none".into()),
            ..ConfigFile::default()
        };
        let (cfg, prov) = resolve(&env, Some(&file), &manifest_with_auth("email")).unwrap();
        assert_eq!(cfg.auth_mode, AuthMode::None);
        assert_eq!(prov.layer("auth_mode"), Some(ConfigLayer::ConfigToml));
    }

    /// The host has the last word. A packaged desktop build and a hosting
    /// platform both need to guarantee a mode across whatever a company's
    /// manifest happens to say.
    #[test]
    fn env_beats_everything_for_auth_mode() {
        let env = MapEnv::new([("OPENCOMPANY_AUTH_MODE", "wallet")]);
        let file = ConfigFile {
            auth_mode: Some("none".into()),
            ..ConfigFile::default()
        };
        let (cfg, prov) = resolve(&env, Some(&file), &manifest_with_auth("email")).unwrap();
        assert_eq!(cfg.auth_mode, AuthMode::Wallet);
        assert_eq!(prov.layer("auth_mode"), Some(ConfigLayer::Env));
    }

    /// Not a silent fallback to email: "the sign-in you configured is not the
    /// one you got" is invisible from a running host, so it fails at boot.
    #[test]
    fn an_unknown_auth_mode_is_a_config_error() {
        let env = MapEnv::new([("OPENCOMPANY_AUTH_MODE", "walet")]);
        let err = resolve(&env, None, &default_manifest()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("email, wallet, none"), "{message}");
        assert!(message.contains("walet"), "{message}");
    }

    #[test]
    fn auth_mode_predicates_match_the_variants() {
        assert!(AuthMode::Email.has_login() && AuthMode::Email.uses_email());
        // A wallet company has a sign-in, but no mailbox anywhere in it.
        assert!(AuthMode::Wallet.has_login() && !AuthMode::Wallet.uses_email());
        assert!(!AuthMode::None.has_login() && !AuthMode::None.uses_email());
    }

    #[test]
    fn config_toml_beats_manifest_for_brain_mode() {
        let env = MapEnv::default();
        let file = ConfigFile {
            brain_mode: Some("sidecar".into()),
            ..ConfigFile::default()
        };
        let (cfg, prov) = resolve(&env, Some(&file), &manifest_with_brain("hosted")).unwrap();
        assert_eq!(cfg.brain_mode, BrainMode::Sidecar);
        assert_eq!(prov.layer("brain_mode"), Some(ConfigLayer::ConfigToml));
    }

    #[test]
    fn manifest_supplies_brain_mode_when_env_and_toml_absent() {
        let env = MapEnv::default();
        let (cfg, prov) = resolve(&env, None, &manifest_with_brain("sidecar")).unwrap();
        assert_eq!(cfg.brain_mode, BrainMode::Sidecar);
        assert_eq!(prov.layer("brain_mode"), Some(ConfigLayer::Manifest));
    }

    #[test]
    fn credential_from_env_enables_cycles() {
        let env = MapEnv::new([("TINYHUMANS_API_KEY", "th_live_abc123")]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();

        assert!(cfg.tinyhumans_credential.is_some());
        assert!(cfg.cycles_available());
        assert_eq!(prov.layer("tinyhumans_credential"), Some(ConfigLayer::Env));
    }

    /// The hosted path: no static key at all, just a platform-projected token
    /// file. Cycles must still be available — the instance can *obtain* a token —
    /// and the source reads `attested`.
    #[test]
    fn projected_token_file_alone_enables_cycles() {
        // The path must actually exist: the projected tier is selected on
        // existence, not on the variable merely being set.
        let dir = tempfile::Builder::new()
            .prefix("oc-cfg-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "projected-token").unwrap();

        let env = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.to_str().unwrap(),
        )]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();

        assert!(cfg.tinyhumans_credential.is_none(), "no static secret held");
        assert_eq!(cfg.tinyhumans_token_file.as_deref(), Some(path.as_path()));
        assert!(cfg.credential_available());
        assert!(cfg.cycles_available());
        assert_eq!(
            cfg.credential_source(),
            crate::company::CredentialSource::Attested
        );
        assert_eq!(prov.layer("tinyhumans_token_file"), Some(ConfigLayer::Env));
    }

    /// The docker case: a leftover `TINYHUMANS_TOKEN_FILE` naming a path nothing
    /// mounted must NOT report an identity this instance cannot present. Reporting
    /// `attested` here would also make `cycles_available` true with no obtainable
    /// bearer, so hosted cognition would be gated on a credential that does not
    /// exist. Regression test for the config surface disagreeing with
    /// `TinyhumansTokenSource::from_env`.
    #[test]
    fn a_token_file_that_does_not_exist_is_not_attested() {
        // A real directory, but the token path inside it is never created: the
        // fixture must name something that does not exist.
        let dir = tempfile::Builder::new()
            .prefix("oc-absent-")
            .tempdir()
            .expect("tempdir");
        let missing = dir.path().join("token");
        assert!(!missing.exists(), "fixture path must not exist");

        let env = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            missing.to_str().unwrap(),
        )]);
        let (cfg, _) = resolve(&env, None, &default_manifest()).unwrap();

        assert_eq!(
            cfg.credential_source(),
            crate::company::CredentialSource::None,
            "an unmounted path must not read as attested"
        );
        assert!(!cfg.credential_available());
        assert!(!cfg.cycles_available());
    }

    /// Same unmounted path, but a static key is present: the source degrades to
    /// the static tier rather than to `none`, matching `from_env`'s fallback.
    #[test]
    fn a_missing_token_file_degrades_to_the_static_tier() {
        // A real directory, but the token path inside it is never created: the
        // fixture must name something that does not exist.
        let dir = tempfile::Builder::new()
            .prefix("oc-absent-")
            .tempdir()
            .expect("tempdir");
        let missing = dir.path().join("token");
        let env = MapEnv::new([
            (
                crate::company::credentials::TOKEN_FILE_ENV,
                missing.to_str().unwrap(),
            ),
            (crate::company::credentials::API_KEY_ENV, "th_static"),
        ]);
        let (cfg, _) = resolve(&env, None, &default_manifest()).unwrap();

        assert_eq!(
            cfg.credential_source(),
            crate::company::CredentialSource::Static
        );
        assert!(cfg.credential_available());
    }

    /// Precedence: a projected file that exists outranks a leftover static key.
    #[test]
    fn projected_file_outranks_a_static_key_for_the_source() {
        let dir = tempfile::Builder::new()
            .prefix("oc-cfg-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "projected-token").unwrap();

        let env = MapEnv::new([
            (
                crate::company::credentials::TOKEN_FILE_ENV,
                path.to_str().unwrap(),
            ),
            (crate::company::credentials::API_KEY_ENV, "th_static"),
        ]);
        let (cfg, _) = resolve(&env, None, &default_manifest()).unwrap();
        assert_eq!(
            cfg.credential_source(),
            crate::company::CredentialSource::Attested
        );

        // Docker development keeps working on the static tier alone.
        let static_only = MapEnv::new([(crate::company::credentials::API_KEY_ENV, "th_static")]);
        let (cfg, _) = resolve(&static_only, None, &default_manifest()).unwrap();
        assert_eq!(
            cfg.credential_source(),
            crate::company::CredentialSource::Static
        );

        // Neither tier configured → nothing obtainable, no cycles.
        let (cfg, _) = resolve(&MapEnv::default(), None, &default_manifest()).unwrap();
        assert_eq!(
            cfg.credential_source(),
            crate::company::CredentialSource::None
        );
        assert!(!cfg.credential_available());
    }

    #[test]
    fn public_url_and_tinyplace_url_resolve_by_precedence() {
        // public_url: env wins; tinyplace_api_url only in config.toml.
        let env = MapEnv::new([("OPENCOMPANY_PUBLIC_URL", "https://public.example")]);
        let file = ConfigFile {
            public_url: Some("https://toml.example".into()),
            tinyplace_api_url: Some("https://tp.toml".into()),
            ..ConfigFile::default()
        };
        let (cfg, prov) = resolve(&env, Some(&file), &default_manifest()).unwrap();

        assert_eq!(cfg.public_url.as_deref(), Some("https://public.example"));
        assert_eq!(prov.layer("public_url"), Some(ConfigLayer::Env));
        assert_eq!(cfg.tinyplace_api_url, "https://tp.toml");
        assert_eq!(
            prov.layer("tinyplace_api_url"),
            Some(ConfigLayer::ConfigToml)
        );
    }

    #[test]
    fn public_url_defaults_to_none() {
        let env = MapEnv::default();
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        assert!(cfg.public_url.is_none());
        assert_eq!(prov.layer("public_url"), Some(ConfigLayer::Default));
    }

    #[test]
    fn credential_from_config_toml_when_env_absent() {
        let env = MapEnv::default();
        let file = ConfigFile {
            tinyhumans_api_key: Some("th_from_toml".into()),
            ..ConfigFile::default()
        };
        let (cfg, prov) = resolve(&env, Some(&file), &default_manifest()).unwrap();
        assert_eq!(
            cfg.tinyhumans_credential.as_ref().unwrap().expose(),
            "th_from_toml"
        );
        assert_eq!(
            prov.layer("tinyhumans_credential"),
            Some(ConfigLayer::ConfigToml)
        );
    }

    #[test]
    fn debug_redacts_secrets() {
        let env = MapEnv::new([
            ("TINYHUMANS_API_KEY", "th_super_secret_value"),
            ("GITHUB_TOKEN", "ghp_secret_token"),
        ]);
        let (cfg, _) = resolve(&env, None, &default_manifest()).unwrap();
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("th_super_secret_value"));
        assert!(!rendered.contains("ghp_secret_token"));
        assert!(rendered.contains("set"));
    }

    #[test]
    fn invalid_brain_mode_is_a_config_error() {
        let env = MapEnv::new([("OPENCOMPANY_BRAIN_MODE", "quantum")]);
        let err = resolve(&env, None, &default_manifest()).unwrap_err();
        assert_eq!(err.code(), "config_error");
        assert!(err.to_string().contains("quantum"));
    }

    #[test]
    fn empty_env_value_is_ignored() {
        let env = MapEnv::new([("OPENCOMPANY_BIND", "")]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        assert_eq!(cfg.bind, DEFAULT_BIND);
        assert_eq!(prov.layer("bind"), Some(ConfigLayer::Default));
    }

    // -----------------------------------------------------------------
    // resolve_serve_bind: the layers `serve` actually honours.
    //
    // Before issue #425 `serve` read only its `--bind` flag, so
    // `OPENCOMPANY_BIND` moved `doctor`'s report but never the listener.
    // `serve_bind_env_beats_config_toml` is the regression test for exactly
    // that: it is red against the flag-only behaviour.
    // -----------------------------------------------------------------

    #[test]
    fn serve_bind_flag_beats_env_and_config_toml() {
        let env = MapEnv::new([("OPENCOMPANY_BIND", "127.0.0.1:2222")]);
        let (bind, source) = resolve_serve_bind(
            Some("127.0.0.1:1111".into()),
            &env,
            Some("127.0.0.1:3333".into()),
        );
        assert_eq!(bind, "127.0.0.1:1111");
        assert_eq!(source, "--bind");
    }

    #[test]
    fn serve_bind_env_beats_config_toml() {
        let env = MapEnv::new([("OPENCOMPANY_BIND", "127.0.0.1:2222")]);
        let (bind, source) = resolve_serve_bind(None, &env, Some("127.0.0.1:3333".into()));
        assert_eq!(bind, "127.0.0.1:2222");
        assert_eq!(source, "OPENCOMPANY_BIND");
    }

    #[test]
    fn serve_bind_empty_env_falls_through() {
        // Same empty-is-unset convention `empty_env_value_is_ignored` pins for
        // the `resolve` chain: an exported-but-blank variable must not shadow
        // the layer beneath it.
        let env = MapEnv::new([("OPENCOMPANY_BIND", "")]);
        let (bind, source) = resolve_serve_bind(None, &env, Some("127.0.0.1:3333".into()));
        assert_eq!(bind, "127.0.0.1:3333");
        assert_eq!(source, "config.toml");

        // With nothing under it either, an empty variable reaches the default.
        let (bind, source) = resolve_serve_bind(None, &env, None);
        assert_eq!(bind, DEFAULT_BIND);
        assert_eq!(source, "default");
    }

    #[test]
    fn serve_bind_config_toml_used_when_no_flag_or_env() {
        let env = MapEnv::default();
        let (bind, source) = resolve_serve_bind(None, &env, Some("127.0.0.1:3333".into()));
        assert_eq!(bind, "127.0.0.1:3333");
        assert_eq!(source, "config.toml");
    }

    #[test]
    fn serve_bind_defaults_to_loopback_when_nothing_set() {
        let env = MapEnv::default();
        let (bind, source) = resolve_serve_bind(None, &env, None);
        assert_eq!(bind, DEFAULT_BIND);
        assert_eq!(source, "default");
        // The default must stay loopback: a wildcard bind is only ever reached
        // by explicit operator intent (flag, variable, or config entry).
        assert!(
            bind.starts_with("127.0.0.1:"),
            "default bind must be loopback"
        );
    }

    #[test]
    fn serve_bind_honours_a_wildcard_only_from_an_explicit_layer() {
        // The hosted manager injects `OPENCOMPANY_BIND=0.0.0.0:8080`; that must
        // reach the listener, and be attributed to the variable.
        let env = MapEnv::new([("OPENCOMPANY_BIND", "0.0.0.0:8080")]);
        let (bind, source) = resolve_serve_bind(None, &env, None);
        assert_eq!(bind, "0.0.0.0:8080");
        assert_eq!(source, "OPENCOMPANY_BIND");
    }

    #[test]
    fn config_file_load_returns_none_when_absent() {
        let dir = std::env::temp_dir().join(format!("oc-cfg-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(ConfigFile::load(&dir).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_file_load_parses_toml() {
        let dir = std::env::temp_dir().join(format!("oc-cfg-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(CONFIG_FILE),
            "brain_mode = \"sidecar\"\napi_url = \"https://x\"\n",
        )
        .unwrap();
        let file = ConfigFile::load(&dir).unwrap().unwrap();
        assert_eq!(file.brain_mode.as_deref(), Some("sidecar"));
        assert_eq!(file.api_url.as_deref(), Some("https://x"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workspace_section_defaults_to_clearing_tmp() {
        // Absent `[workspace]` → default (clear on startup).
        assert!(WorkspaceSection::default().resolve().clear_tmp_on_startup);
        // An explicit opt-out is honored.
        let section = WorkspaceSection {
            clear_tmp_on_startup: Some(false),
            ..WorkspaceSection::default()
        };
        assert!(!section.resolve().clear_tmp_on_startup);
    }

    #[test]
    fn workspace_section_parses_quotas() {
        let section = WorkspaceSection {
            storage_quota_gb: Some(2.0),
            tmp_quota_gb: Some(0.0), // non-positive → unlimited
            ..WorkspaceSection::default()
        };
        let cfg = section.resolve();
        assert_eq!(cfg.storage_quota_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(cfg.tmp_quota_bytes, None);
        // Absent → unlimited.
        assert_eq!(
            WorkspaceSection::default().resolve().storage_quota_bytes,
            None
        );
    }

    #[test]
    fn config_file_parses_workspace_section() {
        let dir = std::env::temp_dir().join(format!("oc-cfg-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(CONFIG_FILE),
            "[workspace]\ngit_enabled = true\nclear_tmp_on_startup = false\n",
        )
        .unwrap();
        let file = ConfigFile::load(&dir).unwrap().unwrap();
        assert_eq!(file.workspace.clear_tmp_on_startup, Some(false));
        assert_eq!(file.workspace.git_enabled, Some(true));
        assert!(!file.workspace.resolve().clear_tmp_on_startup);
        assert!(file.workspace.resolve().git_enabled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_config_file_is_a_config_error() {
        let dir = std::env::temp_dir().join(format!("oc-cfg-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), "not = = valid").unwrap();
        let err = ConfigFile::load(&dir).unwrap_err();
        assert_eq!(err.code(), "config_error");
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // write_config_toml
    // -----------------------------------------------------------------------

    fn write_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("oc-write-{tag}-"))
            .tempdir()
            .expect("tempdir")
    }

    /// Writing into a data root that has no `config.toml` yet — the first-run
    /// case — creates the file, and it parses back through the normal reader.
    #[test]
    fn writing_creates_the_file_when_absent() {
        let dir = write_dir("new");
        let path = write_config_toml(
            dir.path(),
            &[
                ("bind", ConfigValue::Str("0.0.0.0:9000".into())),
                ("auth_mode", ConfigValue::Str("none".into())),
            ],
        )
        .unwrap();

        assert_eq!(path, dir.path().join(CONFIG_FILE));
        let file = ConfigFile::load(dir.path()).unwrap().unwrap();
        assert_eq!(file.bind.as_deref(), Some("0.0.0.0:9000"));
        assert_eq!(file.auth_mode.as_deref(), Some("none"));
    }

    /// The reason this goes through `toml_edit` at all: the shipped file's
    /// commented `[[default_mcp_server]]` PLACEHOLDER block is documentation an
    /// operator is meant to read and uncomment, and serializing a `ConfigFile`
    /// back out would delete it along with every other comment. Untouched keys
    /// must survive too.
    #[test]
    fn writing_preserves_comments_and_untouched_keys() {
        let dir = write_dir("comments");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "# The instance's bind address.\n\
             bind = \"127.0.0.1:8080\"\n\
             api_url = \"https://api.example.test\"\n\
             \n\
             # PLACEHOLDER — uncomment to ship a default tool server.\n\
             # [[default_mcp_server]]\n\
             # name = \"deepwiki\"\n",
        )
        .unwrap();

        write_config_toml(
            dir.path(),
            &[("bind", ConfigValue::Str("0.0.0.0:9000".into()))],
        )
        .unwrap();

        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(text.contains("# The instance's bind address."));
        assert!(text.contains("# PLACEHOLDER — uncomment to ship a default tool server."));
        assert!(text.contains("# [[default_mcp_server]]"));
        assert!(text.contains("# name = \"deepwiki\""));
        assert!(
            text.contains("api_url = \"https://api.example.test\""),
            "an untouched key must survive verbatim"
        );
        assert!(text.contains("0.0.0.0:9000"), "the edit must land");
        assert!(
            !text.contains("127.0.0.1:8080"),
            "the old value must be replaced, not duplicated"
        );
    }

    /// A dotted key writes into `[workspace]`, creating the table when needed,
    /// and the result resolves through `WorkspaceSection`.
    #[test]
    fn writing_reaches_into_the_workspace_table() {
        let dir = write_dir("ws");
        write_config_toml(
            dir.path(),
            &[
                ("workspace.clear_tmp_on_startup", ConfigValue::Bool(false)),
                ("workspace.max_blob_mb", ConfigValue::Float(64.0)),
            ],
        )
        .unwrap();

        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(
            text.contains("[workspace]"),
            "the table header must be explicit, not implicit: {text}"
        );

        let file = ConfigFile::load(dir.path()).unwrap().unwrap();
        assert_eq!(file.workspace.clear_tmp_on_startup, Some(false));
        assert_eq!(file.workspace.max_blob_mb, Some(64.0));
        assert!(!file.workspace.resolve().clear_tmp_on_startup);
    }

    /// `Unset` removes the key rather than writing `""`. The difference matters:
    /// an absent key falls through to the next precedence layer, where a blank
    /// string would be a set-but-empty value.
    #[test]
    fn unset_removes_the_key_so_the_next_layer_applies() {
        let dir = write_dir("unset");
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "auth_mode = \"wallet\"\nbind = \"0.0.0.0:9000\"\n",
        )
        .unwrap();

        write_config_toml(dir.path(), &[("auth_mode", ConfigValue::Unset)]).unwrap();

        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(!text.contains("auth_mode"), "the key must be gone: {text}");

        let file = ConfigFile::load(dir.path()).unwrap().unwrap();
        assert!(file.auth_mode.is_none());
        assert_eq!(file.bind.as_deref(), Some("0.0.0.0:9000"));

        // And with the key gone, resolution falls through to the manifest.
        let mut manifest = default_manifest();
        manifest.users.mode = "wallet".into();
        let (cfg, prov) = resolve(&MapEnv::default(), Some(&file), &manifest).unwrap();
        assert_eq!(cfg.auth_mode, AuthMode::Wallet);
        assert_eq!(prov.layer("auth_mode"), Some(ConfigLayer::Manifest));
    }

    /// Clearing a key out of a `[workspace]` table that does not exist must not
    /// materialize an empty table just to delete nothing out of it.
    #[test]
    fn unset_does_not_materialize_a_missing_table() {
        let dir = write_dir("unset-ws");
        write_config_toml(dir.path(), &[("workspace.max_blob_mb", ConfigValue::Unset)]).unwrap();
        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(!text.contains("[workspace]"), "no empty table: {text}");
    }

    /// Merging into a document that could not be parsed would overwrite whatever
    /// the operator actually had there, so a malformed file is refused — the
    /// same contract `ConfigFile::load` holds.
    #[test]
    fn writing_refuses_a_malformed_existing_file() {
        let dir = write_dir("bad");
        std::fs::write(dir.path().join(CONFIG_FILE), "not = = valid").unwrap();

        let err = write_config_toml(
            dir.path(),
            &[("bind", ConfigValue::Str("0.0.0.0:9000".into()))],
        )
        .unwrap_err();
        assert_eq!(err.code(), "config_error");

        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(text, "not = = valid", "the original must be left alone");
    }

    /// The write is atomic via a same-directory temp file and `rename`. Nothing
    /// may be left behind for the next boot (or the next write) to trip over —
    /// checked by name pattern rather than the old fixed `config.toml.tmp`,
    /// since the temp name is now made unique per call.
    #[test]
    fn writing_leaves_no_temp_file_behind() {
        let dir = write_dir("tmp");
        write_config_toml(
            dir.path(),
            &[("bind", ConfigValue::Str("0.0.0.0:9000".into()))],
        )
        .unwrap();
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "left behind: {leftover:?}");
    }

    /// A write that fails after the temp file's directory disappears out from
    /// under it must not leave anything behind, and must report the failure
    /// rather than panic — the failure half of
    /// `writing_leaves_no_temp_file_behind` above, forced deterministically
    /// (absent-parent, like `store::fs::durable_append_reports_an_unwritable_path`)
    /// rather than by injecting a permission failure, which behaves
    /// differently depending on whether the test runs as root.
    #[test]
    fn a_write_that_fails_leaves_no_temp_file_behind() {
        let dir = write_dir("write-fails");
        let root = dir.path().join("gone");
        // `existing` reads NotFound as "no config yet" and proceeds, so the
        // failure below comes from the temp file's own `std::fs::write`
        // rather than from the initial read.
        let err = write_config_toml(&root, &[("bind", ConfigValue::Str("0.0.0.0:9000".into()))])
            .unwrap_err();
        assert_eq!(err.code(), "config_error");
        assert!(
            !root.exists(),
            "a write into a missing directory must not create it or anything in it"
        );
    }

    /// Two writers racing the same directory must not clobber each other: each
    /// call's edits land, and neither call's temp file collides with the
    /// other's — the bug CodeRabbit flagged on #908 (`config.rs:549`).
    #[test]
    fn concurrent_writes_to_the_same_directory_do_not_clobber_each_other() {
        let dir = write_dir("concurrent");
        let path = dir.path().to_path_buf();

        let a = std::thread::spawn({
            let path = path.clone();
            move || {
                for i in 0..25 {
                    write_config_toml(
                        &path,
                        &[("bind", ConfigValue::Str(format!("0.0.0.0:{}", 9000 + i)))],
                    )
                    .unwrap();
                }
            }
        });
        let b = std::thread::spawn({
            let path = path.clone();
            move || {
                for i in 0..25 {
                    write_config_toml(
                        &path,
                        &[(
                            "public_url",
                            ConfigValue::Str(format!("https://h{i}.example")),
                        )],
                    )
                    .unwrap();
                }
            }
        });
        a.join().unwrap();
        b.join().unwrap();

        // Both threads' edits target different keys, so a clobbered write would
        // show up as one key or the other missing from the final file — not as
        // a torn/unparseable file, which `ConfigFile::load` would already catch.
        let file = ConfigFile::load(&path).unwrap().unwrap();
        assert!(file.bind.is_some(), "the bind writer's edits went missing");
        assert!(
            file.public_url.is_some(),
            "the public_url writer's edits went missing"
        );

        // No stray temp file from either racer.
        let leftover: Vec<_> = std::fs::read_dir(&path)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "left behind: {leftover:?}");
    }

    /// Setup completion is recorded in the file, so "has this instance been set
    /// up" survives a new browser and travels with the data root.
    #[test]
    fn setup_completion_round_trips() {
        let dir = write_dir("done");
        assert!(
            ConfigFile::load(dir.path()).unwrap().is_none(),
            "a fresh data root has no config at all"
        );

        write_config_toml(
            dir.path(),
            &[("setup_completed_at", ConfigValue::Int(1_755_000_000_000))],
        )
        .unwrap();

        let file = ConfigFile::load(dir.path()).unwrap().unwrap();
        assert_eq!(file.setup_completed_at, Some(1_755_000_000_000));
    }
}
