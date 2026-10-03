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
    let dir = tempfile::Builder::new()
        .prefix("oc-doctor-")
        .tempdir()
        .expect("tempdir");
    let path = dir.path().join("token");
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
}

/// A leftover `TINYHUMANS_TOKEN_FILE` naming an unmounted path must not tell
/// an operator the instance is attested — doctor is the surface they check to
/// confirm the projection actually landed, so a false `attested` there is
/// worse than an honest `none`.
#[test]
fn a_token_file_that_does_not_exist_does_not_report_attested() {
    // A real directory, but the token path inside it is never created: the
    // fixture must name something that does not exist.
    let dir = tempfile::Builder::new()
        .prefix("oc-doctor-absent-")
        .tempdir()
        .expect("tempdir");
    let missing = dir.path().join("token");
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
}
