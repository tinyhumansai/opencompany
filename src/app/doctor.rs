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
        "tinyplace_api_url" => cfg.tinyplace_api_url.clone(),
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
    "tinyplace_api_url",
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

    let tinyplace = DoctorCapability {
        name: "tinyplace",
        available: true,
        needs: String::new(),
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
        capabilities: vec![cycles, openhuman, tinyplace, github],
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::app::config::{MapEnv, resolve};
    use crate::company::CompanyManifest;

    fn default_manifest() -> CompanyManifest {
        toml::from_str("[company]\nname = \"X\"\n").expect("valid manifest")
    }

    fn cap<'a>(report: &'a DoctorReport, name: &str) -> &'a DoctorCapability {
        report
            .capabilities
            .iter()
            .find(|c| c.name == name)
            .expect("capability present")
    }

    fn value<'a>(report: &'a DoctorReport, name: &str) -> &'a str {
        report
            .values
            .iter()
            .find(|v| v.name == name)
            .map(|v| v.value.as_str())
            .expect("value present")
    }

    #[test]
    fn cycles_unavailable_without_credential() {
        let env = MapEnv::default();
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);

        let cycles = cap(&report, "cycles");
        assert!(!cycles.available);
        assert!(
            cycles.needs.contains("TINYHUMANS_TOKEN_FILE")
                && cycles.needs.contains("TINYHUMANS_API_KEY"),
            "both tiers must be named: {}",
            cycles.needs
        );
        assert_eq!(value(&report, "credential_source"), "none");
        assert_eq!(value(&report, "tinyhumans_token_file"), "unset");
    }

    /// The hosted path: a projected token file and no static key. Cycles are
    /// available, the tier is named `attested`, and the path (not a secret) is
    /// printed so an operator can confirm the projection landed.
    #[test]
    fn projected_token_file_reports_the_attested_tier() {
        // A real file: the attested tier is selected on the path existing, not on
        // the variable being set, so a fixture path would report `none` here.
        let dir = std::env::temp_dir().join(format!("oc-doctor-{}", crate::ports::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "projected-token").unwrap();

        let env = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.to_str().unwrap(),
        )]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);

        assert!(cap(&report, "cycles").available);
        assert_eq!(value(&report, "credential_source"), "attested");
        assert_eq!(value(&report, "tinyhumans_credential"), "missing");
        assert_eq!(
            value(&report, "tinyhumans_token_file"),
            path.to_str().unwrap()
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A leftover `TINYHUMANS_TOKEN_FILE` naming an unmounted path must not tell
    /// an operator the instance is attested — doctor is the surface they check to
    /// confirm the projection actually landed, so a false `attested` there is
    /// worse than an honest `none`.
    #[test]
    fn a_token_file_that_does_not_exist_does_not_report_attested() {
        let missing =
            std::env::temp_dir().join(format!("oc-doctor-absent-{}", crate::ports::generate_id()));
        let env = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            missing.to_str().unwrap(),
        )]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);

        assert!(!cap(&report, "cycles").available);
        assert_eq!(value(&report, "credential_source"), "none");
    }

    #[test]
    fn static_key_reports_the_static_tier() {
        let env = MapEnv::new([(crate::company::credentials::API_KEY_ENV, "th_secret")]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);
        assert_eq!(value(&report, "credential_source"), "static");
    }

    #[test]
    fn cycles_available_with_hosted_credential() {
        let env = MapEnv::new([("TINYHUMANS_API_KEY", "th_secret")]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);

        let cycles = cap(&report, "cycles");
        assert!(cycles.available);
        assert!(cycles.needs.is_empty());
    }

    #[test]
    fn cycles_needs_hosted_when_credential_set_but_sidecar() {
        let env = MapEnv::new([
            ("TINYHUMANS_API_KEY", "th_secret"),
            ("OPENCOMPANY_BRAIN_MODE", "sidecar"),
        ]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);

        let cycles = cap(&report, "cycles");
        assert!(!cycles.available);
        assert!(cycles.needs.contains("brain_mode = hosted"));
    }

    #[test]
    fn report_never_leaks_secret_bytes() {
        let env = MapEnv::new([
            ("TINYHUMANS_API_KEY", "th_super_secret_value"),
            ("GITHUB_TOKEN", "ghp_super_secret_token"),
        ]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);

        let text = report.to_text();
        let json = serde_json::to_string(&report).unwrap();
        for rendered in [&text, &json] {
            assert!(!rendered.contains("th_super_secret_value"));
            assert!(!rendered.contains("ghp_super_secret_token"));
        }
        // The credential is present, so it renders as `set`.
        assert!(text.contains("set"));
    }

    #[test]
    fn values_report_layer_labels() {
        let env = MapEnv::new([("OPENCOMPANY_BIND", "0.0.0.0:9000")]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);

        let bind = report.values.iter().find(|v| v.name == "bind").unwrap();
        assert_eq!(bind.value, "0.0.0.0:9000");
        assert_eq!(bind.layer, "env");

        let api = report.values.iter().find(|v| v.name == "api_url").unwrap();
        assert_eq!(api.layer, "default");
    }

    #[test]
    fn openhuman_and_github_capabilities_track_config() {
        let env = MapEnv::new([
            ("OPENCOMPANY_OPENHUMAN_URL", "http://127.0.0.1:7777"),
            ("GITHUB_TOKEN", "ghp_x"),
        ]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        let report = report(&cfg, &prov);

        assert!(cap(&report, "openhuman").available);
        assert!(cap(&report, "github").available);
        assert!(cap(&report, "tinyplace").available);
    }
}
