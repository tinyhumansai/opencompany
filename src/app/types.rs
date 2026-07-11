use std::path::PathBuf;

use serde::Serialize;

use crate::runtime::CompanyRegistry;
use crate::{VERSION, tiny::RuntimeModuleStatus};

/// Runtime configuration for OpenCompany.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Address for the Axum HTTP server.
    pub bind: String,
    /// Optional sibling OpenHuman checkout used by launcher commands.
    pub openhuman_root: Option<PathBuf>,
    /// Bearer token required on operator routes. When `None`, Phase-1 dev mode
    /// allows local operator calls without authentication.
    pub operator_token: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_string(),
            openhuman_root: None,
            operator_token: None,
        }
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
}
