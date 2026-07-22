//! Precedence-resolved runtime configuration.
//!
//! [`RuntimeConfig`] is assembled from four layers, earlier winning over later:
//!
//! 1. Environment variables (`OPENCOMPANY_*`, `TINYHUMANS_*`, `TINYPLACE_*`,
//!    `GITHUB_TOKEN`).
//! 2. `~/.opencompany/config.toml`.
//! 3. The company manifest (`[brain].mode`).
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
    /// Returns the value for `key`, or `None` when unset or empty.
    fn get(&self, key: &str) -> Option<String>;
}

/// Reads from the real process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        match std::env::var(key) {
            Ok(value) if !value.is_empty() => Some(value),
            _ => None,
        }
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
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).filter(|value| !value.is_empty()).cloned()
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
    /// The `[workspace]` section: data-dir layout lifecycle knobs.
    pub workspace: WorkspaceSection,
}

/// The `[workspace]` section of `config.toml`: lifecycle of the canonical
/// data-dir layout (see [`DataLayout`](crate::store::DataLayout)).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WorkspaceSection {
    /// Empty the ephemeral `tmp/` scratch directory on startup. Default: true.
    pub clear_tmp_on_startup: Option<bool>,
}

impl WorkspaceSection {
    /// Resolves the section against its defaults.
    pub fn resolve(&self) -> WorkspaceConfig {
        WorkspaceConfig {
            clear_tmp_on_startup: self.clear_tmp_on_startup.unwrap_or(true),
        }
    }
}

/// Resolved `[workspace]` configuration.
#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    /// Whether the ephemeral `tmp/` scratch is cleared on startup.
    pub clear_tmp_on_startup: bool,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            clear_tmp_on_startup: true,
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
    /// Resolved `[workspace]` data-dir layout configuration.
    pub workspace: WorkspaceConfig,
}

impl RuntimeConfig {
    /// True when hosted cognition can run: hosted mode plus a credential.
    pub fn cycles_available(&self) -> bool {
        self.brain_mode == BrainMode::Hosted && self.tinyhumans_credential.is_some()
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
            .field("openhuman_url", &self.openhuman_url)
            .field("tinyplace_api_url", &self.tinyplace_api_url)
            .field("public_url", &self.public_url)
            .field("github_token", &redacted(&self.github_token))
            .field(
                "tinyhumans_credential",
                &redacted(&self.tinyhumans_credential),
            )
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
        env.get("TINYHUMANS_API_KEY"),
        config_toml.and_then(|c| c.tinyhumans_api_key.clone()),
    )
    .map(SecretValue);

    let workspace = config_toml
        .map(|c| c.workspace.resolve())
        .unwrap_or_default();

    let config = RuntimeConfig {
        bind,
        data_dir: PathBuf::from(data_dir),
        api_url,
        brain_mode,
        openhuman_url,
        tinyplace_api_url,
        public_url,
        github_token,
        tinyhumans_credential,
        workspace,
    };
    Ok((config, prov))
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
    match std::env::var_os("OPENCOMPANY_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => match std::env::var_os("HOME") {
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
        };
        assert!(!section.resolve().clear_tmp_on_startup);
    }

    #[test]
    fn config_file_parses_workspace_section() {
        let dir = std::env::temp_dir().join(format!("oc-cfg-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(CONFIG_FILE),
            "[workspace]\nclear_tmp_on_startup = false\n",
        )
        .unwrap();
        let file = ConfigFile::load(&dir).unwrap().unwrap();
        assert_eq!(file.workspace.clear_tmp_on_startup, Some(false));
        assert!(!file.workspace.resolve().clear_tmp_on_startup);
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
}
