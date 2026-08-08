//! The local runtime behind the packaged OpenCompany desktop application.
//!
//! The desktop app is a thin Tauri shell around the existing React console and
//! Axum operator API.  It binds only loopback, persists under the app-data
//! directory supplied by Tauri, and starts from one of the shipped company
//! presets.  There is intentionally no OpenHuman local-AI preset bridge here:
//! OpenCompany owns its local company store and its company presets.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Serialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::company::CompanyManifest;
use crate::runtime::{RuntimeBuilder, company_id_from_name};
use crate::server::cors::CorsConfig;
use crate::{AppConfig, AppState, Result};

/// A company definition bundled into the desktop app.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct DesktopPreset {
    /// Stable identifier used by the desktop host.
    pub id: &'static str,
    /// Human-readable company template name.
    pub name: &'static str,
    manifest: &'static str,
}

macro_rules! preset {
    ($id:literal, $name:literal) => {
        DesktopPreset {
            id: $id,
            name: $name,
            manifest: include_str!(concat!("../companies/", $id, "/company.toml")),
        }
    };
}

/// The product company templates shipped with the desktop app.
pub const PRESETS: &[DesktopPreset] = &[
    preset!("agentic_accounting_firm", "Agentic Accounting Firm"),
    preset!("agentic_consultation_firm", "Agentic Consultation Firm"),
    preset!("agentic_customer_support", "Agentic Customer Support"),
    preset!("agentic_design_studio", "Agentic Design Studio"),
    preset!("agentic_enterprise_sales", "Agentic Enterprise Sales"),
    preset!("agentic_game_business", "Agentic Game Business"),
    preset!("agentic_game_studio", "Agentic Game Studio"),
    preset!("agentic_influencer_business", "Agentic Influencer Business"),
    preset!("agentic_law_firm", "Agentic Law Firm"),
    preset!("agentic_marketing_agency", "Agentic Marketing Agency"),
    preset!("agentic_media_company", "Agentic Media Company"),
    preset!("agentic_pharma_startup", "Agentic Pharma Startup"),
    preset!("agentic_realestate_company", "Agentic Real Estate Company"),
    preset!("agentic_recruiting_company", "Agentic Recruiting Company"),
    preset!("agentic_software_company", "Agentic Software Company"),
    preset!("agentic_venture_capital", "Agentic Venture Capital"),
    preset!("agentic_venture_studio", "Agentic Venture Studio"),
    preset!("signals_opportunity_studio", "Signals Opportunity Studio"),
    preset!("startup_accelerator", "Startup Accelerator"),
];

/// The preset a first-run desktop install uses.
pub const DEFAULT_PRESET_ID: &str = "agentic_marketing_agency";
/// The local standing invite used only to bootstrap the desktop webview session.
pub const DESKTOP_OPERATOR_EMAIL: &str = "operator@opencompany.local";
/// The origin used by Tauri v2's desktop webview.
pub const TAURI_WEBVIEW_ORIGIN: &str = "http://tauri.localhost";

/// Finds a bundled preset by its stable id.
pub fn preset(id: &str) -> Option<&'static DesktopPreset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// Information the webview needs to authenticate to its embedded runtime.
#[derive(Clone, Debug, Serialize)]
pub struct DesktopConfig {
    pub api_url: String,
    pub company: String,
    pub operator_email: &'static str,
}

/// A running local OpenCompany API. Dropping it aborts the loopback server.
pub struct DesktopRuntime {
    config: DesktopConfig,
    server: JoinHandle<std::io::Result<()>>,
}

impl DesktopRuntime {
    pub fn config(&self) -> &DesktopConfig {
        &self.config
    }
}

impl Drop for DesktopRuntime {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Starts an offline, loopback-only runtime from a bundled preset.
pub async fn start_local(home: impl Into<PathBuf>, preset_id: &str) -> Result<DesktopRuntime> {
    let preset = preset(preset_id).ok_or_else(|| {
        crate::OpenCompanyError::Config(format!("unknown desktop preset `{preset_id}`"))
    })?;
    let mut manifest: CompanyManifest = toml::from_str(preset.manifest).map_err(|error| {
        crate::OpenCompanyError::Config(format!(
            "bundled desktop preset `{preset_id}` is invalid: {error}"
        ))
    })?;
    // A desktop install has no mail transport. This local-only address is a
    // standing invite solely so the shell can redeem a loopback magic link; it
    // never leaves the device or grant access to a network caller.
    if !manifest
        .users
        .admins
        .iter()
        .any(|email| email == DESKTOP_OPERATOR_EMAIL)
    {
        manifest
            .users
            .admins
            .push(DESKTOP_OPERATOR_EMAIL.to_string());
    }

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address: SocketAddr = listener.local_addr()?;
    let company_id = company_id_from_name(&manifest.company.name);
    let state = AppState::new(AppConfig {
        bind: address.to_string(),
        ..AppConfig::default()
    })
    .with_home(home.into())
    .with_cors(CorsConfig {
        allowed_origins: vec![TAURI_WEBVIEW_ORIGIN.to_string()],
    });
    let runtime = RuntimeBuilder::new(state.home().to_path_buf(), manifest)
        .with_id(company_id.clone())
        .build()
        .await?;
    state
        .registry()
        .insert(company_id.clone(), std::sync::Arc::new(runtime));

    let app = crate::server::router(state);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(DesktopRuntime {
        config: DesktopConfig {
            api_url: format!("http://{address}"),
            company: company_id.as_ref().to_string(),
            operator_email: DESKTOP_OPERATOR_EMAIL,
        },
        server,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ships_the_full_company_template_catalog() {
        assert_eq!(PRESETS.len(), 19);
        assert_eq!(
            preset(DEFAULT_PRESET_ID).unwrap().name,
            "Agentic Marketing Agency"
        );
        assert!(PRESETS.iter().all(|preset| !preset.manifest.is_empty()));
    }

    #[tokio::test]
    async fn starts_a_loopback_runtime_from_the_default_preset() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = start_local(directory.path(), DEFAULT_PRESET_ID)
            .await
            .unwrap();
        assert!(runtime.config().api_url.starts_with("http://127.0.0.1:"));
        assert_eq!(runtime.config().operator_email, DESKTOP_OPERATOR_EMAIL);
    }
}
