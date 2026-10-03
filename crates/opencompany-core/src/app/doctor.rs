//! `opencompany doctor` — explain the effective configuration.
//!
//! [`report`] turns a resolved [`RuntimeConfig`] and its [`ConfigProvenance`]
//! into a serializable [`DoctorReport`]: every effective value with the layer
//! that set it, plus a per-capability section stating what is available and
//! what is missing. Secrets are only ever surfaced as `set`/`missing`; the
//! report never carries credential bytes.

use serde::Serialize;

use crate::app::config::{ConfigLayer, ConfigProvenance, RuntimeConfig, redacted};

/// One effective configuration value with the layer that set it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorValue {
    /// The config field name (e.g. `api_url`).
    pub name: &'static str,
    /// The rendered value. Secrets render as `set`/`missing`.
    pub value: String,
    /// The layer that set this value (`env`, `config.toml`, `manifest`,
    /// `default`).
    pub layer: &'static str,
}

/// A capability and whether it is currently available.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorCapability {
    /// Capability name (e.g. `cycles`).
    pub name: &'static str,
    /// Whether the capability can run with the current configuration.
    pub available: bool,
    /// When unavailable, what the operator must set; empty when available.
    pub needs: String,
}

/// The full doctor report: effective values plus capability readiness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    /// Effective configuration values, in stable field order.
    pub values: Vec<DoctorValue>,
    /// Per-capability readiness.
    pub capabilities: Vec<DoctorCapability>,
}

impl DoctorReport {
    /// Renders the report as a human-readable, aligned text block.
    pub fn to_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        out.push_str("Configuration\n");
        let width = self.values.iter().map(|v| v.name.len()).max().unwrap_or(0);
        for value in &self.values {
            let _ = writeln!(
                out,
                "  {:<width$}  {}  [{}]",
                value.name,
                value.value,
                value.layer,
                width = width
            );
        }
        out.push_str("\nCapabilities\n");
        for cap in &self.capabilities {
            if cap.available {
                let _ = writeln!(out, "  {:<12} available", cap.name);
            } else {
                let _ = writeln!(out, "  {:<12} unavailable: {}", cap.name, cap.needs);
            }
        }
        out
    }
}

/// The value string for a config field, resolving secrets to `set`/`missing`.
fn value_of(cfg: &RuntimeConfig, field: &str) -> String {
    match field {
        "bind" => cfg.bind.clone(),
        "data_dir" => cfg.data_dir.display().to_string(),
        "api_url" => cfg.api_url.clone(),
        "brain_mode" => cfg.brain_mode.to_string(),
        "openhuman_url" => cfg.openhuman_url.clone().unwrap_or_else(|| "unset".into()),
        "github_token" => redacted(&cfg.github_token).to_string(),
        "tinyhumans_credential" => redacted(&cfg.tinyhumans_credential).to_string(),
        // A path, not a secret — printing it is how an operator confirms the pod
        // was handed a projected identity at all.
        "tinyhumans_token_file" => cfg
            .tinyhumans_token_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unset".into()),
        other => format!("<unknown field {other}>"),
    }
}

/// The stable field order shown by the doctor.
const FIELDS: &[&str] = &[
    "bind",
    "data_dir",
    "api_url",
    "brain_mode",
    "openhuman_url",
    "github_token",
    "tinyhumans_credential",
    "tinyhumans_token_file",
];

/// The name of the derived row naming the active credential tier.
const CREDENTIAL_SOURCE_FIELD: &str = "credential_source";

/// Builds a [`DoctorReport`] from resolved config and its provenance.
pub fn report(cfg: &RuntimeConfig, prov: &ConfigProvenance) -> DoctorReport {
    let mut values: Vec<DoctorValue> = FIELDS
        .iter()
        .map(|&field| DoctorValue {
            name: field,
            value: value_of(cfg, field),
            layer: prov.layer(field).unwrap_or(ConfigLayer::Default).label(),
        })
        .collect();

    // The active tier, spelled out. Two rows above say what is set; this one says
    // which of them the outbound bearer will actually come from — and it is a
    // tier name, never a credential.
    let source = cfg.credential_source();
    values.push(DoctorValue {
        name: CREDENTIAL_SOURCE_FIELD,
        value: source.to_string(),
        // The derived row inherits the layer of whichever field won the tier.
        layer: match source {
            crate::company::CredentialSource::Attested => prov.layer("tinyhumans_token_file"),
            crate::company::CredentialSource::Static => prov.layer("tinyhumans_credential"),
            // The doctor reports the *instance's* tier, resolved from process
            // config. A company key is per-company state in a secret store, so
            // it can never win here — and if it somehow did, no config layer
            // named it, which is exactly what `None` says.
            crate::company::CredentialSource::Company => None,
            crate::company::CredentialSource::None => None,
        }
        .unwrap_or(ConfigLayer::Default)
        .label(),
    });

    let cycles = DoctorCapability {
        name: "cycles",
        available: cfg.cycles_available(),
        needs: if cfg.cycles_available() {
            String::new()
        } else if !cfg.credential_available() {
            format!(
                "needs {} (hosted) or {}",
                crate::company::credentials::TOKEN_FILE_ENV,
                crate::company::credentials::API_KEY_ENV
            )
        } else {
            format!("needs brain_mode = hosted (currently {})", cfg.brain_mode)
        },
    };

    let openhuman = DoctorCapability {
        name: "openhuman",
        available: cfg.openhuman_url.is_some(),
        needs: if cfg.openhuman_url.is_some() {
            String::new()
        } else {
            "needs OPENCOMPANY_OPENHUMAN_URL".into()
        },
    };

    let github = DoctorCapability {
        name: "github",
        available: cfg.github_token.is_some(),
        needs: if cfg.github_token.is_some() {
            String::new()
        } else {
            "needs GITHUB_TOKEN".into()
        },
    };

    DoctorReport {
        values,
        capabilities: vec![cycles, openhuman, github],
    }
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
