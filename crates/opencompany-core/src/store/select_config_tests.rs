use super::*;
use crate::app::config::MapEnv;

#[test]
fn parses_storage_kinds() {
    assert_eq!("fs".parse::<StorageKind>().unwrap(), StorageKind::Fs);
    assert_eq!(
        "sqlite".parse::<StorageKind>().unwrap(),
        StorageKind::Sqlite
    );
    assert_eq!(
        "MongoDB".parse::<StorageKind>().unwrap(),
        StorageKind::Mongodb
    );
    assert!("postgres".parse::<StorageKind>().is_err());
}

/// Issue #752: only MongoDB keeps secret material off the container's own
/// disk, so only MongoDB clears the repository-credential gates.
#[test]
fn only_mongodb_keeps_secrets_off_the_local_disk() {
    assert!(StorageKind::Fs.secrets_are_plaintext_on_disk());
    assert!(StorageKind::Sqlite.secrets_are_plaintext_on_disk());
    assert!(!StorageKind::Mongodb.secrets_are_plaintext_on_disk());
    // The default is the refusing side: a host that never resolved a
    // backend must not be treated as one that keeps secrets safely.
    assert!(StorageKind::default().secrets_are_plaintext_on_disk());
}

/// The refusal has to be actionable on its own — an operator reading it in
/// a console toast has nothing else to go on.
#[test]
fn the_refusal_names_the_condition_and_both_remedies() {
    let message = plaintext_secret_refusal(StorageKind::Fs);
    assert!(message.contains("OPENCOMPANY_STORAGE=fs"), "{message}");
    assert!(message.contains("OPENCOMPANY_STORAGE=mongodb"), "{message}");
    assert!(message.contains("OPENCOMPANY_MONGODB_URI"), "{message}");
    assert!(message.contains("`repo` grant"), "{message}");
    assert!(message.contains("plaintext"), "{message}");
    // The named kind is the one actually in force, not a hard-coded "fs".
    assert!(plaintext_secret_refusal(StorageKind::Sqlite).contains("OPENCOMPANY_STORAGE=sqlite"),);
}

#[tokio::test]
async fn fs_selection_uses_builder_defaults() {
    let settings = StorageSettings::default();
    let handles = open_storage(&settings, Path::new("/tmp")).await.unwrap();
    assert!(handles.is_none());
}

#[test]
fn parses_memory_backends() {
    assert_eq!(
        "store".parse::<MemoryBackend>().unwrap(),
        MemoryBackend::Store
    );
    assert_eq!("".parse::<MemoryBackend>().unwrap(), MemoryBackend::Store);
    assert_eq!(
        "remote".parse::<MemoryBackend>().unwrap(),
        MemoryBackend::Remote
    );
    assert_eq!(
        "NULL".parse::<MemoryBackend>().unwrap(),
        MemoryBackend::Null
    );
    assert!("redis".parse::<MemoryBackend>().is_err());
}

#[test]
fn the_removed_embedded_spellings_refuse_to_parse() {
    // The in-pod engines (`tinycortex`/`cortex`/`embedded`) were removed
    // with the `tinycortex` feature. A deployment still setting
    // `OPENCOMPANY_MEMORY=tinycortex` must fail loudly at boot, not be
    // quietly reinterpreted as a mode that no longer exists.
    for value in ["tinycortex", "cortex", "embedded"] {
        assert!(
            value.parse::<MemoryBackend>().is_err(),
            "{value} must no longer parse as a memory backend"
        );
    }
}

#[test]
fn each_backend_reports_its_wire_name() {
    // A client reading status should never have to know both names.
    assert_eq!(MemoryBackend::Store.as_str(), "store");
    assert_eq!(MemoryBackend::Remote.as_str(), "remote");
    assert_eq!(MemoryBackend::Null.as_str(), "null");
}

#[test]
fn the_parse_refusal_names_every_accepted_value() {
    let error = "redis".parse::<MemoryBackend>().err().unwrap().to_string();
    for value in ["store", "remote", "null"] {
        assert!(error.contains(value), "{value} missing from: {error}");
    }
}

#[test]
fn settings_debug_never_renders_a_credential() {
    // `StorageSettings` is printed at boot, so a derived `Debug` would put a
    // memory credential and a MongoDB connection string in the startup log
    // of every tenant container.
    let settings = StorageSettings {
        mongodb_uri: Some("mongodb://user:hunter2@cluster.example/db".into()),
        memory_url: Some("https://memory.internal.example".into()),
        memory_api_key: Some("sk-memory-super-secret".into()),
        ..StorageSettings::default()
    };
    let rendered = format!("{settings:?}");
    assert!(!rendered.contains("hunter2"), "{rendered}");
    assert!(!rendered.contains("sk-memory-super-secret"), "{rendered}");
    assert!(!rendered.contains("memory.internal.example"), "{rendered}");
    // Still useful: it says the values are configured.
    assert!(rendered.contains("<set>"), "{rendered}");
}

#[cfg(feature = "tinymemory")]
#[test]
fn remote_without_a_key_refuses_at_open() {
    let settings = StorageSettings {
        memory_backend: MemoryBackend::Remote,
        memory_driver: Some("cortexdb".into()),
        ..StorageSettings::default()
    };
    let error = open_memory_overlay(&settings)
        .expect_err("remote without a credential must refuse")
        .to_string();
    assert!(error.contains("OPENCOMPANY_MEMORY_API_KEY"), "{error}");
}

#[cfg(feature = "tinymemory")]
#[test]
fn a_retired_engine_refuses_at_open_by_name() {
    let settings = StorageSettings {
        memory_backend: MemoryBackend::Remote,
        memory_driver: Some("supermemory".into()),
        memory_api_key: Some("k".into()),
        ..StorageSettings::default()
    };
    let error = open_memory_overlay(&settings)
        .expect_err("a v1-only engine must refuse")
        .to_string();
    assert!(error.contains("no longer supported"), "{error}");
}

#[cfg(feature = "tinymemory")]
#[test]
fn remote_with_full_config_proceeds_to_the_driver_without_an_acceptance_flag() {
    // The unproven-remote acceptance flag is retired: the vendored
    // tinymemory now ships a driver conformance suite that runs against
    // all the hosted adapters, so the flag's premise is gone. A fully
    // configured remote proceeds to driver construction — the error here
    // is the driver failing to reach the (nonexistent) endpoint or an
    // admission refusal, never a demand for a deleted knob.
    let settings = StorageSettings {
        memory_backend: MemoryBackend::Remote,
        memory_driver: Some("cortexdb".into()),
        memory_url: Some("https://memory.invalid".into()),
        memory_api_key: Some("k".into()),
        ..StorageSettings::default()
    };
    match open_memory_overlay(&settings) {
        // `Ok(None)` is the trap this arm exists to close: it is how a
        // silently skipped remote overlay would look, and an
        // error-message-only assertion would pass straight through it.
        Ok(overlay) => assert!(
            overlay.is_some(),
            "a fully configured remote must bind an overlay, not skip one"
        ),
        Err(error) => {
            let error = error.to_string();
            assert!(
                !error.contains("ALLOW_UNPROVEN_REMOTE"),
                "the retired knob must not be demanded: {error}"
            );
        }
    }
}

#[cfg(feature = "tinymemory")]
#[test]
fn remote_binds_and_reports_its_driver() {
    // The success half of the pair above: a complete configuration binds,
    // and the descriptor it reports back names the driver that was asked
    // for rather than a fallback. No acceptance step is involved — that
    // knob is retired.
    let settings = StorageSettings {
        memory_backend: MemoryBackend::Remote,
        memory_driver: Some("cortexdb".into()),
        memory_url: Some("https://memory.example".into()),
        memory_api_key: Some("k".into()),
        ..StorageSettings::default()
    };
    let overlay = open_memory_overlay(&settings)
        .expect("a fully configured remote engine binds")
        .expect("remote yields an overlay");
    assert_eq!(overlay.descriptor.backend, MemoryBackend::Remote);
    assert_eq!(overlay.descriptor.driver_id, "cortexdb");
    // Binding is offline: nothing has probed yet, and claiming health
    // before a probe would be the same lie in the other direction.
    assert_eq!(
        overlay.descriptor.healthy, None,
        "bind must not pre-claim health"
    );
    assert!(
        overlay.probe.is_some(),
        "the provider-seam overlay must carry a probe handle for the boot health check"
    );
}

#[cfg(feature = "tinymemory")]
#[test]
fn remote_cortexdb_binds_and_reports_its_driver() {
    // `OPENCOMPANY_MEMORY=remote OPENCOMPANY_MEMORY_DRIVER=cortexdb` with a
    // URL and a key must open the same way the other three hosted drivers
    // do: offline (construction validates shape, not reachability) and
    // reporting the driver id it was asked for.
    let settings = StorageSettings {
        memory_backend: MemoryBackend::Remote,
        memory_driver: Some("cortexdb".into()),
        memory_url: Some("http://127.0.0.1:3141".into()),
        memory_api_key: Some("k".into()),
        ..StorageSettings::default()
    };
    let overlay = open_memory_overlay(&settings)
        .expect("a fully configured cortexdb engine binds")
        .expect("remote yields an overlay");
    assert_eq!(overlay.descriptor.backend, MemoryBackend::Remote);
    assert_eq!(overlay.descriptor.driver_id, "cortexdb");
    assert_eq!(
        overlay.descriptor.healthy, None,
        "bind must not pre-claim health"
    );
}

#[cfg(feature = "tinymemory")]
#[tokio::test]
async fn refresh_health_records_the_probe_answer() {
    // `null` is the one driver whose health is deterministic offline —
    // its `health()` is `Ready` by contract — so it proves the probe
    // path end-to-end with no network: probe handle → `refresh_health`
    // → `descriptor.healthy`, the value `/spec` serves.
    let settings = StorageSettings {
        memory_backend: MemoryBackend::Null,
        ..StorageSettings::default()
    };
    let mut overlay = open_memory_overlay(&settings)
        .expect("null binds")
        .expect("null yields an overlay");
    assert_eq!(overlay.descriptor.healthy, None);
    overlay
        .refresh_health(std::time::Duration::from_secs(5))
        .await;
    assert_eq!(
        overlay.descriptor.healthy,
        Some(true),
        "the probe's answer must land on the descriptor"
    );
}

#[cfg(feature = "tinymemory")]
#[test]
fn the_gate_applies_only_to_remote() {
    // `null` retains nothing by design, so it is not routing memory at an
    // unproven third party and is not behind this gate.
    let settings = StorageSettings {
        memory_backend: MemoryBackend::Null,
        ..StorageSettings::default()
    };
    assert!(
        open_memory_overlay(&settings).is_ok(),
        "null must not be gated on the remote-adapter assertion"
    );
}

#[cfg(feature = "tinymemory")]
#[test]
fn null_opens_and_reports_itself() {
    let settings = StorageSettings {
        memory_backend: MemoryBackend::Null,
        ..StorageSettings::default()
    };
    let overlay = open_memory_overlay(&settings)
        .unwrap()
        .expect("null binds an overlay");
    assert_eq!(overlay.descriptor.backend, MemoryBackend::Null);
    assert_eq!(overlay.descriptor.driver_id, "null");
    // A bound provider serves all three seam partitions, not just facts.
    // Asserting each one separately is what catches a partition that is
    // wired to `None` at the construction site while the others are not —
    // which reads downstream as "this engine has no scratch", not as a bug.
    assert!(overlay.facts.is_some(), "a provider serves facts too");
    assert!(
        overlay.inbound_context.is_some(),
        "a provider serves the inbound-context partition"
    );
    assert!(
        overlay.scratch.is_some(),
        "a provider serves the scratch partition"
    );
}

#[cfg(not(feature = "tinymemory"))]
#[test]
fn the_provider_modes_require_the_feature() {
    for backend in [MemoryBackend::Remote, MemoryBackend::Null] {
        let settings = StorageSettings {
            memory_backend: backend,
            ..StorageSettings::default()
        };
        let error = open_memory_overlay(&settings).err().unwrap().to_string();
        assert!(error.contains("`tinymemory` feature"), "{error}");
    }
}

#[test]
fn default_memory_backend_is_store() {
    assert_eq!(
        StorageSettings::default().memory_backend,
        MemoryBackend::Store
    );
    // Store is the no-op: no overlay, base backend keeps its own memory.
    assert!(
        open_memory_overlay(&StorageSettings::default())
            .unwrap()
            .is_none()
    );
}

#[test]
fn from_env_reads_the_remote_memory_knobs() {
    // The four knobs `remote` needs. Without this, a rename in `from_env`
    // would surface as "the engine refuses and names a variable you did
    // set", which reads as a broken deployment rather than a broken parse.
    const KEYS: [&str; 3] = [
        "OPENCOMPANY_MEMORY_DRIVER",
        "OPENCOMPANY_MEMORY_URL",
        "OPENCOMPANY_MEMORY_API_KEY",
    ];
    let env = MapEnv::new([
        (KEYS[0], "supermemory"),
        (KEYS[1], "https://memory.example"),
        (KEYS[2], "sk-test"),
    ]);
    let settings = StorageSettings::from_env_source(&env).unwrap();
    assert_eq!(settings.memory_driver.as_deref(), Some("supermemory"));
    assert_eq!(
        settings.memory_url.as_deref(),
        Some("https://memory.example")
    );
    assert_eq!(settings.memory_api_key.as_deref(), Some("sk-test"));

    // Empty is absent, not an empty credential: `require` would otherwise
    // accept a blank key and defer the failure to the first call.
    let blank =
        StorageSettings::from_env_source(&MapEnv::new([(KEYS[0], ""), (KEYS[2], "")])).unwrap();
    assert_eq!(blank.memory_driver, None);
    assert_eq!(blank.memory_api_key, None);

    let unset = StorageSettings::from_env_source(&MapEnv::default()).unwrap();
    assert_eq!(unset.memory_driver, None);
    assert_eq!(unset.memory_url, None);
    assert_eq!(unset.memory_api_key, None);
}

#[test]
fn from_env_reads_memory_backend() {
    let env = MapEnv::new([("OPENCOMPANY_MEMORY", "remote")]);
    assert_eq!(
        StorageSettings::from_env_source(&env)
            .unwrap()
            .memory_backend,
        MemoryBackend::Remote
    );

    assert_eq!(
        StorageSettings::from_env_source(&MapEnv::default())
            .unwrap()
            .memory_backend,
        MemoryBackend::Store
    );
}

#[test]
fn from_env_reads_tenant_id() {
    let env = MapEnv::new([("OPENCOMPANY_TENANT_ID", "acme")]);
    assert_eq!(
        StorageSettings::from_env_source(&env)
            .unwrap()
            .tenant_id
            .as_deref(),
        Some("acme")
    );

    // An empty value is filtered out, same as the mongodb vars.
    assert_eq!(
        StorageSettings::from_env_source(&MapEnv::new([("OPENCOMPANY_TENANT_ID", "")]))
            .unwrap()
            .tenant_id,
        None
    );

    // Unset leaves it `None` (the id-namespacing no-op).
    assert_eq!(
        StorageSettings::from_env_source(&MapEnv::default())
            .unwrap()
            .tenant_id,
        None
    );
}

#[test]
fn from_env_reads_data_dir() {
    let env = MapEnv::new([("OPENCOMPANY_DATA_DIR", "/srv/oc-data")]);
    assert_eq!(
        StorageSettings::from_env_source(&env).unwrap().data_dir,
        Some(PathBuf::from("/srv/oc-data")),
        "OPENCOMPANY_DATA_DIR must be read into StorageSettings::data_dir"
    );
}

#[test]
fn from_env_reads_allow_ephemeral_memory() {
    const KEY: &str = "OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL";
    assert!(
        !StorageSettings::from_env_source(&MapEnv::default())
            .unwrap()
            .allow_ephemeral_memory
    );

    // Truthy values set the durability assertion.
    for truthy in ["1", "true", "YES", "On"] {
        assert!(
            StorageSettings::from_env_source(&MapEnv::new([(KEY, truthy)]))
                .unwrap()
                .allow_ephemeral_memory,
            "{truthy:?} must read as durability asserted"
        );
    }

    // Any non-truthy value stays false (fails safe toward refusal).
    for falsy in ["0", "false", "no", ""] {
        assert!(
            !StorageSettings::from_env_source(&MapEnv::new([(KEY, falsy)]))
                .unwrap()
                .allow_ephemeral_memory,
            "{falsy:?} must read as not asserted"
        );
    }
}

#[test]
fn with_memory_config_from_resolves_ownership_from_the_injected_source() {
    let section = crate::app::config::MemorySection {
        backend: Some("remote".into()),
        driver: Some("supermemory".into()),
        url: Some("https://memory.example".into()),
        ..Default::default()
    };

    // The injected source owns the choice: the `config.toml` layer is
    // inert, exactly as it is for a deployment env naming an engine.
    let env = MapEnv::new([("OPENCOMPANY_MEMORY", "store")]);
    let settings = StorageSettings::from_env_source(&env)
        .unwrap()
        .with_memory_config_from(&env, &section)
        .unwrap();
    assert_eq!(settings.memory_backend, MemoryBackend::Store);
    assert_eq!(settings.memory_driver, None);
    assert_eq!(settings.memory_url, None);

    // No injected ownership: the file layer is applied.
    let unset = StorageSettings::from_env_source(&MapEnv::default())
        .unwrap()
        .with_memory_config_from(&MapEnv::default(), &section)
        .unwrap();
    assert_eq!(unset.memory_backend, MemoryBackend::Remote);
    assert_eq!(unset.memory_driver.as_deref(), Some("supermemory"));
    assert_eq!(unset.memory_url.as_deref(), Some("https://memory.example"));
}

#[cfg(feature = "mongodb")]
#[tokio::test]
async fn mongodb_selection_requires_uri() {
    let settings = StorageSettings {
        kind: StorageKind::Mongodb,
        ..Default::default()
    };
    let error = open_storage(&settings, std::path::Path::new("/tmp"))
        .await
        .expect_err("mongodb without a URI must refuse")
        .to_string();
    assert!(error.contains("OPENCOMPANY_MONGODB_URI"), "{error}");
}
/// The bundle-command refusals, executed — the #1279 review neutralised
/// the bin-resident versions with `if false &&` and nothing went red;
/// these are the tests that make that mutation fail.
#[test]
fn bundle_env_refusals_fire_and_the_fs_default_passes() {
    // fs+store default: both flag spellings pass — no regression.
    let default = StorageSettings::default();
    refuse_bundle_env(&default, false).expect("default env, no flag");
    refuse_bundle_env(&default, true).expect("default env, explicit --home");

    // null refuses in both directions regardless of the flag.
    let null = StorageSettings {
        memory_backend: MemoryBackend::Null,
        ..StorageSettings::default()
    };
    for flagged in [false, true] {
        let err = refuse_bundle_env(&null, flagged)
            .expect_err("null must refuse")
            .to_string();
        assert!(err.contains("OPENCOMPANY_MEMORY=null"), "{err}");
    }

    // A live environment refuses an explicit --home (two deployments in
    // one bundle) but proceeds without the flag.
    let live = StorageSettings {
        kind: StorageKind::Mongodb,
        ..StorageSettings::default()
    };
    let err = refuse_bundle_env(&live, true)
        .expect_err("live env + --home must refuse")
        .to_string();
    assert!(err.contains("--home"), "{err}");
    refuse_bundle_env(&live, false).expect("live env without the flag proceeds");

    // Tenant mode refuses on a live env; whitespace does not count as set.
    let tenant = StorageSettings {
        kind: StorageKind::Mongodb,
        tenant_id: Some("acme".into()),
        ..StorageSettings::default()
    };
    let err = refuse_bundle_env(&tenant, false)
        .expect_err("tenant mode must refuse")
        .to_string();
    assert!(err.contains("OPENCOMPANY_TENANT_ID"), "{err}");
    let blank_tenant = StorageSettings {
        kind: StorageKind::Mongodb,
        tenant_id: Some("  ".into()),
        ..StorageSettings::default()
    };
    refuse_bundle_env(&blank_tenant, false).expect("blank tenant id is unset");
}
