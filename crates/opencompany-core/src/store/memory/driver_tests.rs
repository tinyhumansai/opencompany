use super::*;

fn remote(driver: &str, url: Option<&str>, key: Option<&str>) -> MemoryDriverConfig {
    MemoryDriverConfig {
        mode: MemoryMode::Remote,
        driver_id: Some(driver.to_string()),
        url: url.map(str::to_string),
        api_key: key.map(str::to_string),
        data_dir: None,
    }
}

fn refusal(config: &MemoryDriverConfig) -> String {
    match open_driver(config) {
        Ok(_) => panic!("expected {config:?} to be refused"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn null_mode_binds_the_null_engine() {
    let config = MemoryDriverConfig {
        mode: MemoryMode::Null,
        driver_id: None,
        url: None,
        api_key: None,
        data_dir: None,
    };
    let engine = open_driver(&config).unwrap();
    assert_eq!(engine.descriptor().id, super::super::null::NULL_ENGINE_ID);
}

#[test]
fn the_supported_set_is_upstreams_registry() {
    assert_eq!(supported_remote_engines(), vec!["cortexdb", "tinyhumans"]);
}

#[test]
fn cortexdb_binds_with_an_https_endpoint_and_a_key() {
    let engine = open_driver(&remote(
        "cortexdb",
        Some("https://cortex.example"),
        Some("k"),
    ))
    .unwrap();
    assert_eq!(engine.descriptor().id, "cortexdb");
}

#[test]
fn the_v1_cortex_alias_still_binds_cortexdb() {
    let engine = open_driver(&remote("cortex", Some("https://cortex.example"), Some("k"))).unwrap();
    assert_eq!(engine.descriptor().id, "cortexdb");
}

#[test]
fn retired_engines_are_refused_by_name() {
    for retired in ["supermemory", "mem0", "Cognee"] {
        let why = refusal(&remote(retired, Some("https://x.example"), Some("k")));
        assert!(why.contains("no longer supported"), "{retired}: {why}");
    }
}

#[test]
fn an_unknown_engine_lists_what_is_available() {
    let why = refusal(&remote("nope", Some("https://x.example"), Some("k")));
    assert!(why.contains("cortexdb"), "{why}");
}

#[test]
fn remote_mode_requires_a_driver_and_a_key() {
    let mut missing_driver = remote("cortexdb", Some("https://x.example"), Some("k"));
    missing_driver.driver_id = None;
    assert!(refusal(&missing_driver).contains("OPENCOMPANY_MEMORY_DRIVER"));
    assert!(
        refusal(&remote("cortexdb", Some("https://x.example"), None))
            .contains("OPENCOMPANY_MEMORY_API_KEY")
    );
}

#[test]
fn a_credentialed_plain_http_endpoint_is_refused_unless_loopback() {
    let why = refusal(&remote(
        "cortexdb",
        Some("http://cortex.example"),
        Some("k"),
    ));
    assert!(why.contains("https"), "{why}");
    assert!(
        open_driver(&remote(
            "cortexdb",
            Some("http://127.0.0.1:9000"),
            Some("k")
        ))
        .is_ok()
    );
}

#[test]
fn debug_never_prints_the_endpoint_or_the_credential() {
    let rendered = format!(
        "{:?}",
        remote("cortexdb", Some("https://secret-host"), Some("sk-secret"))
    );
    assert!(!rendered.contains("secret-host"));
    assert!(!rendered.contains("sk-secret"));
}
