use super::*;
use crate::company::CompanyManifest;

pub(super) fn manifest_with_brain(mode: &str) -> CompanyManifest {
    let toml_src = format!("[company]\nname = \"X\"\n[brain]\nmode = \"{mode}\"\n");
    toml::from_str(&toml_src).expect("valid manifest")
}

pub(super) fn default_manifest() -> CompanyManifest {
    toml::from_str("[company]\nname = \"X\"\n").expect("valid manifest")
}

#[test]
pub(super) fn defaults_fill_in_when_nothing_set() {
    let env = MapEnv::default();
    let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();

    assert_eq!(cfg.api_url, DEFAULT_API_URL);
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
pub(super) fn data_dir_from_treats_empty_as_unset() {
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
pub(super) fn resolve_propagates_workspace_to_runtime_config() {
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
pub(super) fn default_mcp_servers_resolve_from_config_toml_and_are_normalized() {
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
pub(super) fn default_mcp_servers_parse_from_the_toml_array_of_tables() {
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
pub(super) fn env_beats_config_toml_beats_manifest_beats_default() {
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

pub(super) fn manifest_with_auth(mode: &str) -> CompanyManifest {
    let toml_src = format!("[company]\nname = \"X\"\n[users]\nmode = \"{mode}\"\n");
    toml::from_str(&toml_src).expect("valid manifest")
}

/// A manifest naming no mode signs people in by email — which is what every
/// company did before the mode existed, so no deployment changes behaviour
/// by upgrading.
#[test]
pub(super) fn auth_mode_defaults_to_email() {
    let env = MapEnv::default();
    let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
    assert_eq!(cfg.auth_mode, AuthMode::Email);
    // The manifest always supplies one (serde fills the default), exactly
    // as it does for the brain mode.
    assert_eq!(prov.layer("auth_mode"), Some(ConfigLayer::Manifest));
}

#[test]
pub(super) fn manifest_supplies_auth_mode_when_env_and_toml_absent() {
    let env = MapEnv::default();
    let (cfg, prov) = resolve(&env, None, &manifest_with_auth("wallet")).unwrap();
    assert_eq!(cfg.auth_mode, AuthMode::Wallet);
    assert_eq!(prov.layer("auth_mode"), Some(ConfigLayer::Manifest));
}

#[test]
pub(super) fn config_toml_beats_the_manifest_for_auth_mode() {
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
pub(super) fn env_beats_everything_for_auth_mode() {
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
pub(super) fn an_unknown_auth_mode_is_a_config_error() {
    let env = MapEnv::new([("OPENCOMPANY_AUTH_MODE", "walet")]);
    let err = resolve(&env, None, &default_manifest()).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("email, wallet, none"), "{message}");
    assert!(message.contains("walet"), "{message}");
}

#[test]
pub(super) fn auth_mode_predicates_match_the_variants() {
    assert!(AuthMode::Email.has_login() && AuthMode::Email.uses_email());
    // A wallet company has a sign-in, but no mailbox anywhere in it.
    assert!(AuthMode::Wallet.has_login() && !AuthMode::Wallet.uses_email());
    assert!(!AuthMode::None.has_login() && !AuthMode::None.uses_email());
}

#[test]
pub(super) fn config_toml_beats_manifest_for_brain_mode() {
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
pub(super) fn manifest_supplies_brain_mode_when_env_and_toml_absent() {
    let env = MapEnv::default();
    let (cfg, prov) = resolve(&env, None, &manifest_with_brain("sidecar")).unwrap();
    assert_eq!(cfg.brain_mode, BrainMode::Sidecar);
    assert_eq!(prov.layer("brain_mode"), Some(ConfigLayer::Manifest));
}

#[test]
pub(super) fn credential_from_env_enables_cycles() {
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
pub(super) fn projected_token_file_alone_enables_cycles() {
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
pub(super) fn a_token_file_that_does_not_exist_is_not_attested() {
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
pub(super) fn a_missing_token_file_degrades_to_the_static_tier() {
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
pub(super) fn projected_file_outranks_a_static_key_for_the_source() {
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
pub(super) fn public_url_resolves_by_precedence() {
    // public_url: env wins over config.toml.
    let env = MapEnv::new([("OPENCOMPANY_PUBLIC_URL", "https://public.example")]);
    let file = ConfigFile {
        public_url: Some("https://toml.example".into()),
        ..ConfigFile::default()
    };
    let (cfg, prov) = resolve(&env, Some(&file), &default_manifest()).unwrap();

    assert_eq!(cfg.public_url.as_deref(), Some("https://public.example"));
    assert_eq!(prov.layer("public_url"), Some(ConfigLayer::Env));
}

#[test]
pub(super) fn public_url_defaults_to_none() {
    let env = MapEnv::default();
    let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
    assert!(cfg.public_url.is_none());
    assert_eq!(prov.layer("public_url"), Some(ConfigLayer::Default));
}

#[test]
pub(super) fn credential_from_config_toml_when_env_absent() {
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
pub(super) fn debug_redacts_secrets() {
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
pub(super) fn invalid_brain_mode_is_a_config_error() {
    let env = MapEnv::new([("OPENCOMPANY_BRAIN_MODE", "quantum")]);
    let err = resolve(&env, None, &default_manifest()).unwrap_err();
    assert_eq!(err.code(), "config_error");
    assert!(err.to_string().contains("quantum"));
}

#[test]
pub(super) fn empty_env_value_is_ignored() {
    let env = MapEnv::new([("OPENCOMPANY_BIND", "")]);
    let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
    assert_eq!(cfg.bind, DEFAULT_BIND);
    assert_eq!(prov.layer("bind"), Some(ConfigLayer::Default));
}

// -----------------------------------------------------------------
// resolve_base_url (AD-001 / AD-014): a hosted tenant is handed its
// whole environment by the platform that provisions it, so an unset
// api_url must refuse to boot instead of silently
// becoming production. Every other deployment kind is unaffected — the
// pinned decision in `defaults_fill_in_when_nothing_set` above still
// holds for an undeclared (self-hosted) process.
// -----------------------------------------------------------------

pub(super) fn hosted_tenant_env<const N: usize>(pairs: [(&str, &str); N]) -> MapEnv {
    let mut all = vec![("OPENCOMPANY_DEPLOYMENT", "hosted-tenant")];
    all.extend(pairs);
    MapEnv::new(all)
}

#[test]
pub(super) fn hosted_tenant_refuses_to_boot_with_no_api_url() {
    let env = hosted_tenant_env([]);
    let err = resolve(&env, None, &default_manifest()).unwrap_err();
    assert_eq!(err.code(), "config_error");
    let message = err.to_string();
    assert!(message.contains("TINYHUMANS_API_URL"), "{message}");
}

#[test]
pub(super) fn hosted_tenant_treats_an_empty_api_url_as_unset() {
    let env = hosted_tenant_env([("TINYHUMANS_API_URL", "   ")]);
    let err = resolve(&env, None, &default_manifest()).unwrap_err();
    assert!(err.to_string().contains("TINYHUMANS_API_URL"));
}

#[test]
pub(super) fn hosted_tenant_uses_an_explicitly_set_api_url_unchanged() {
    let env = hosted_tenant_env([("TINYHUMANS_API_URL", "https://staging-api.tinyhumans.ai")]);
    let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
    assert_eq!(cfg.api_url, "https://staging-api.tinyhumans.ai");
    assert_eq!(prov.layer("api_url"), Some(ConfigLayer::Env));
}

/// A hosted tenant may also state the URL in `config.toml` rather than
/// the environment — the gate is "was it stated", not "which layer".
#[test]
pub(super) fn hosted_tenant_accepts_api_url_from_config_toml() {
    let env = hosted_tenant_env([]);
    let file = ConfigFile {
        api_url: Some("https://staging-api.tinyhumans.ai".into()),
        ..ConfigFile::default()
    };
    let (cfg, prov) = resolve(&env, Some(&file), &default_manifest()).unwrap();
    assert_eq!(cfg.api_url, "https://staging-api.tinyhumans.ai");
    assert_eq!(prov.layer("api_url"), Some(ConfigLayer::ConfigToml));
}

/// The other side of the split: self-hosted and desktop deployments keep
/// defaulting. Forcing every plain `serve` to name a backend it never
/// had to before would break the documented zero-config quickstart for a
/// deployment kind that owns the choice by construction.
#[test]
pub(super) fn self_hosted_and_desktop_still_default_api_url_when_unset() {
    for kind in ["self-hosted", ""] {
        let env = MapEnv::new([("OPENCOMPANY_DEPLOYMENT", kind)]);
        let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
        assert_eq!(cfg.api_url, DEFAULT_API_URL, "deployment={kind:?}");
        assert_eq!(prov.layer("api_url"), Some(ConfigLayer::Default));
    }

    let env = MapEnv::new([("OPENCOMPANY_DEPLOYMENT", "desktop")]);
    let (cfg, prov) = resolve(&env, None, &default_manifest()).unwrap();
    assert_eq!(cfg.api_url, DEFAULT_API_URL);
    assert_eq!(prov.layer("api_url"), Some(ConfigLayer::Default));
}

/// The tenant-namespace inference (`OPENCOMPANY_TENANT_ID` alone, no
/// explicit `OPENCOMPANY_DEPLOYMENT`) names a hosted tenant too — see
/// `Deployment::from_env` — so it must gate the same as an explicit
/// declaration rather than being read as self-hosted.
#[test]
pub(super) fn tenant_namespace_alone_also_gates_as_hosted_tenant() {
    let env = MapEnv::new([("OPENCOMPANY_TENANT_ID", "acme")]);
    let err = resolve(&env, None, &default_manifest()).unwrap_err();
    assert!(err.to_string().contains("TINYHUMANS_API_URL"));
}
