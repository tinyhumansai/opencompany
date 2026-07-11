use std::path::PathBuf;

use serde::Serialize;

use crate::app::config::{BrainMode, redacted};
use crate::ports::types::SecretValue;
use crate::runtime::CompanyRegistry;
use crate::{VERSION, tiny::RuntimeModuleStatus};

/// Runtime configuration for OpenCompany.
///
/// `Debug` is implemented by hand so the TinyHumans credential is redacted to
/// `set`/`missing` and can never reach a log line or panic message.
#[derive(Clone)]
pub struct AppConfig {
    /// Address for the Axum HTTP server.
    pub bind: String,
    /// Optional sibling OpenHuman checkout used by launcher commands.
    pub openhuman_root: Option<PathBuf>,
    /// Bearer token required on operator routes. When `None`, Phase-1 dev mode
    /// allows local operator calls without authentication.
    pub operator_token: Option<String>,
    /// TinyHumans orchestration API base URL.
    pub api_url: String,
    /// Which brain the runtime drives.
    pub brain_mode: BrainMode,
    /// TinyHumans hosted-brain credential, if configured. Redacted in `Debug`.
    pub tinyhumans_credential: Option<SecretValue>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_string(),
            openhuman_root: None,
            operator_token: None,
            api_url: crate::app::config::DEFAULT_API_URL.to_string(),
            brain_mode: BrainMode::Hosted,
            tinyhumans_credential: None,
        }
    }
}

impl AppConfig {
    /// True when hosted cognition can run: hosted mode plus a credential.
    pub fn cycles_available(&self) -> bool {
        self.brain_mode == BrainMode::Hosted && self.tinyhumans_credential.is_some()
    }
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppConfig")
            .field("bind", &self.bind)
            .field("openhuman_root", &self.openhuman_root)
            .field(
                "operator_token",
                &self.operator_token.as_ref().map(|_| "set"),
            )
            .field("api_url", &self.api_url)
            .field("brain_mode", &self.brain_mode)
            .field(
                "tinyhumans_credential",
                &redacted(&self.tinyhumans_credential),
            )
            .finish()
    }
}

/// Shared application state passed to Axum handlers.
#[derive(Clone, Debug)]
pub struct AppState {
    config: AppConfig,
    registry: CompanyRegistry,
}

impl AppState {
    /// Builds state from runtime configuration with an empty company registry.
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            registry: CompanyRegistry::new(),
        }
    }

    /// Returns runtime configuration.
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// The registry of running companies served by this host.
    pub fn registry(&self) -> &CompanyRegistry {
        &self.registry
    }

    /// Returns a serializable system specification snapshot.
    pub fn spec(&self) -> AppSpec {
        AppSpec {
            name: "opencompany",
            version: VERSION,
            framework: "axum",
            modules: vec![
                "app",
                "company",
                "ports",
                "store",
                "policy",
                "brain",
                "runtime",
                "server",
                "openhuman",
                "tiny",
            ],
            runtime_modules: RuntimeModuleStatus::all(),
            openhuman_root: self
                .config
                .openhuman_root
                .as_ref()
                .map(|path| path.display().to_string()),
            api_url: self.config.api_url.clone(),
            cycles_available: self.config.cycles_available(),
        }
    }
}

/// Serializable OpenCompany runtime specification.
#[derive(Clone, Debug, Serialize)]
pub struct AppSpec {
    /// Crate name.
    pub name: &'static str,
    /// Crate version.
    pub version: &'static str,
    /// HTTP framework used by this host.
    pub framework: &'static str,
    /// First-class source modules.
    pub modules: Vec<&'static str>,
    /// Runtime module integration status.
    pub runtime_modules: Vec<RuntimeModuleStatus>,
    /// Configured OpenHuman checkout path, if any.
    pub openhuman_root: Option<String>,
    /// TinyHumans orchestration API base URL.
    pub api_url: String,
    /// Whether hosted cognition can run (hosted brain plus a credential). No
    /// secret bytes are surfaced.
    pub cycles_available: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_binds_locally() {
        assert_eq!(AppConfig::default().bind, "127.0.0.1:8080");
    }

    #[test]
    fn spec_reports_axum_framework() {
        let spec = AppState::new(AppConfig::default()).spec();

        assert_eq!(spec.framework, "axum");
        assert!(spec.modules.contains(&"server"));
    }

    #[test]
    fn debug_redacts_the_credential() {
        let config = AppConfig {
            tinyhumans_credential: Some(SecretValue("th_super_secret_value".into())),
            ..AppConfig::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("th_super_secret_value"));
        assert!(rendered.contains("set"));
    }

    #[test]
    fn default_config_cannot_run_cycles() {
        assert!(!AppConfig::default().cycles_available());
    }

    #[test]
    fn hosted_with_credential_can_run_cycles() {
        let config = AppConfig {
            tinyhumans_credential: Some(SecretValue("th_secret".into())),
            ..AppConfig::default()
        };
        assert!(config.cycles_available());
        assert!(AppState::new(config).spec().cycles_available);
    }
}
