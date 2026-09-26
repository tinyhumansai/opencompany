use super::*;

/// Shared with [`super::store_tests`], which builds the same declarations and
/// then resolves them against a store.
pub(super) fn server(name: &str, endpoint: &str) -> McpServer {
    McpServer {
        name: name.to_string(),
        endpoint: endpoint.to_string(),
        description: None,
        command: None,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        read_only_tools: Vec::new(),
        timeout_secs: 30,
        enabled: true,
        auth_secret: None,
    }
}

// ---- merge precedence -------------------------------------------------

#[test]
fn effective_unions_manifest_and_runtime() {
    let manifest = vec![server("notion", "https://notion.example/mcp")];
    let runtime = vec![server("linear", "https://linear.example/mcp")];
    let eff = effective_mcp_servers(&[], &manifest, &runtime);
    let names: Vec<&str> = eff.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["notion", "linear"]);
    assert_eq!(eff[0].source, McpSource::Manifest);
    assert_eq!(eff[1].source, McpSource::Runtime);
}

#[test]
fn runtime_overrides_manifest_but_keeps_manifest_source() {
    let manifest = vec![server("notion", "https://notion.example/mcp")];
    let mut override_entry = server("notion", "https://notion.example/mcp");
    override_entry.enabled = false;
    override_entry.allowed_tools = vec!["search".into()];
    let eff = effective_mcp_servers(&[], &manifest, &[override_entry]);
    assert_eq!(eff.len(), 1, "override does not duplicate the server");
    assert_eq!(eff[0].source, McpSource::Manifest, "still manifest-badged");
    assert!(!eff[0].enabled, "override wins the enabled flag");
    assert_eq!(eff[0].allowed_tools, vec!["search".to_string()]);
}

// ---- install defaults, the third merge layer (issue #527) -------------

#[test]
fn a_default_ships_enabled_with_its_own_badge() {
    // The acceptance criterion: a fresh install has the server active with
    // no user action and no company.toml edit.
    let defaults = vec![server("deepwiki", "https://deepwiki.example/mcp")];
    let eff = effective_mcp_servers(&defaults, &[], &[]);
    assert_eq!(eff.len(), 1);
    assert_eq!(eff[0].name, "deepwiki");
    assert!(eff[0].enabled, "a default is active without user action");
    assert_eq!(
        eff[0].source,
        McpSource::Default,
        "not Manifest — nobody wrote it into this company"
    );
}

#[test]
fn no_defaults_leaves_the_two_layer_result_untouched() {
    // The compatibility property every existing install depends on: an
    // install that configures no defaults resolves exactly as before.
    let manifest = vec![server("notion", "https://notion.example/mcp")];
    let runtime = vec![server("linear", "https://linear.example/mcp")];
    let eff = effective_mcp_servers(&[], &manifest, &runtime);
    let names: Vec<&str> = eff.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["notion", "linear"]);
    assert_eq!(eff[0].source, McpSource::Manifest);
    assert_eq!(eff[1].source, McpSource::Runtime);
}

#[test]
fn the_manifest_shadows_a_default_of_the_same_name() {
    // A company that declares the server has said something specific about
    // it; the install-wide default is the fallback it overrides.
    let defaults = vec![server("shared", "https://default.example/mcp")];
    let manifest = vec![server("shared", "https://manifest.example/mcp")];
    let eff = effective_mcp_servers(&defaults, &manifest, &[]);
    assert_eq!(eff.len(), 1, "one row, not two claiming the same slug");
    assert_eq!(eff[0].endpoint, "https://manifest.example/mcp");
    assert_eq!(eff[0].source, McpSource::Manifest);
}

#[test]
fn a_runtime_override_disables_a_default_but_keeps_its_badge() {
    // This is how an operator turns a shipped default off. It must persist
    // as an override rather than a deletion, because the declaration lives
    // in the install config where the console cannot reach it.
    let defaults = vec![server("deepwiki", "https://deepwiki.example/mcp")];
    let mut off = server("deepwiki", "https://deepwiki.example/mcp");
    off.enabled = false;
    let eff = effective_mcp_servers(&defaults, &[], &[off]);
    assert_eq!(eff.len(), 1, "the override does not duplicate the server");
    assert!(!eff[0].enabled, "the operator's disable wins");
    assert_eq!(
        eff[0].source,
        McpSource::Default,
        "still default-badged, so the console still refuses to delete it"
    );
}

#[test]
fn ordering_puts_manifest_first_then_defaults_then_runtime_only() {
    let defaults = vec![server("d", "https://d.example/mcp")];
    let manifest = vec![server("m", "https://m.example/mcp")];
    let runtime = vec![server("r", "https://r.example/mcp")];
    let eff = effective_mcp_servers(&defaults, &manifest, &runtime);
    let names: Vec<&str> = eff.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["m", "d", "r"]);
    assert_eq!(eff[2].source, McpSource::Runtime);
}

// ---- normalizing the configured defaults (issue #527) -----------------

#[test]
fn an_empty_default_list_is_authoritative() {
    // "Ship no defaults" — never "fall back to a built-in set". There is no
    // compiled-in list to fall back to, and adding one later must not
    // change this.
    let (kept, problems) = normalize_default_servers(&[]);
    assert!(kept.is_empty());
    assert!(problems.is_empty());
}

#[test]
fn one_bad_entry_does_not_cost_the_good_ones() {
    let raw = vec![
        server("good", "https://good.example/mcp"),
        server("", "https://nameless.example/mcp"),
        server("alsogood", "https://also.example/mcp"),
    ];
    let (kept, problems) = normalize_default_servers(&raw);
    let names: Vec<&str> = kept.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["good", "alsogood"]);
    assert_eq!(problems.len(), 1, "and the drop is explained, not silent");
}

#[test]
fn a_credential_in_the_endpoint_query_string_is_refused_not_scrubbed() {
    // Refused, because scrubbing would ship a server whose auth silently no
    // longer works — failing at an agent's first tool call instead of here.
    let raw = vec![
        server("qs", "https://api.example.com/mcp?apiKey=leaked"),
        server(
            "qs2",
            "https://api.example.com/mcp?projectId=p&token=leaked",
        ),
        server("qs3", "https://api.example.com/mcp?access_token=leaked"),
        server("fine", "https://api.example.com/mcp?projectId=p"),
    ];
    let (kept, problems) = normalize_default_servers(&raw);
    let names: Vec<&str> = kept.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["fine"], "a benign query parameter is kept");
    assert_eq!(problems.len(), 3);
}

#[test]
fn an_encoded_credential_key_in_the_query_string_is_refused() {
    // Percent-decode each key before matching, or `api%4Bey` (apiKey) sails
    // past the raw-name scan and ships a secret-bearing default.
    let raw = vec![
        server("enc-apikey", "https://api.example.com/mcp?api%4Bey=secret"),
        server("enc-token", "https://api.example.com/mcp?tok%65n=secret"),
        server(
            "enc-access-token",
            "https://api.example.com/mcp?access%2Dtoken=secret",
        ),
        server("fine", "https://api.example.com/mcp?project%5Fid=p"),
    ];
    let (kept, problems) = normalize_default_servers(&raw);
    let names: Vec<&str> = kept.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["fine"], "a decoded-benign key is kept");
    assert_eq!(problems.len(), 3);
}

#[test]
fn a_default_may_not_depend_on_a_credential_key() {
    // A default is handed to every agent on the install unprompted, so it
    // has to work unattended. One that needs auth belongs per company.
    let mut needs_auth = server("private", "https://private.example/mcp");
    needs_auth.auth_secret = Some("mcp/private/auth".to_string());
    let (kept, problems) = normalize_default_servers(&[needs_auth]);
    assert!(kept.is_empty());
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("auth_secret"));
}

#[test]
fn a_non_http_or_stdio_default_is_refused_by_the_shared_validator() {
    // Delegated to `validate_one`, so defaults and every other declaration
    // path enforce the hosted-v1 transport boundary identically.
    let mut stdio = server("stdio", "");
    stdio.command = Some("npx some-mcp-server".to_string());
    let raw = vec![
        stdio,
        server("ftp", "ftp://files.example/mcp"),
        server("ok", "http://localhost:9000/mcp"),
    ];
    let (kept, problems) = normalize_default_servers(&raw);
    let names: Vec<&str> = kept.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["ok"]);
    assert!(!problems.is_empty());
}

#[test]
fn a_userinfo_credential_in_a_default_endpoint_is_refused() {
    let raw = vec![server("ui", "https://user:pass@host.example/mcp")];
    let (kept, problems) = normalize_default_servers(&raw);
    assert!(kept.is_empty(), "the user:pass@host form never ships");
    assert_eq!(problems.len(), 1);
}

#[test]
fn a_duplicated_default_name_keeps_the_first() {
    // Two rows claiming one slug would let merge order decide which won.
    let raw = vec![
        server("dup", "https://first.example/mcp"),
        server("dup", "https://second.example/mcp"),
    ];
    let (kept, problems) = normalize_default_servers(&raw);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].endpoint, "https://first.example/mcp");
    assert_eq!(problems.len(), 1);
}

#[test]
fn a_clean_default_survives_with_its_fields_intact() {
    let mut full = server("full", "https://full.example/mcp");
    full.description = Some("a documentation server".to_string());
    full.allowed_tools = vec!["read".to_string()];
    full.timeout_secs = 45;
    let (kept, problems) = normalize_default_servers(&[full]);
    assert!(problems.is_empty());
    assert_eq!(kept.len(), 1);
    assert_eq!(
        kept[0].description.as_deref(),
        Some("a documentation server")
    );
    assert_eq!(kept[0].allowed_tools, vec!["read".to_string()]);
    assert_eq!(kept[0].timeout_secs, 45);
}

// ---- validation -------------------------------------------------------

#[test]
fn valid_http_server_passes() {
    assert!(validate_servers(&[server("notion", "https://notion.example/mcp")]).is_empty());
}

#[test]
fn duplicate_names_are_rejected() {
    let problems = validate_servers(&[
        server("dup", "https://a.example/mcp"),
        server("dup", "https://b.example/mcp"),
    ]);
    assert!(
        problems.iter().any(|p| p.contains("more than once")),
        "{problems:?}"
    );
}

#[test]
fn non_http_endpoint_is_rejected() {
    let problems = validate_servers(&[server("bad", "ftp://x.example/mcp")]);
    assert!(problems.iter().any(|p| p.contains("http")), "{problems:?}");
}

#[test]
fn missing_endpoint_is_rejected() {
    let problems = validate_servers(&[server("bare", "")]);
    assert!(
        problems.iter().any(|p| p.contains("endpoint")),
        "{problems:?}"
    );
}

#[test]
fn stdio_command_is_rejected_in_hosted_v1() {
    let mut s = server("local", "https://x.example/mcp");
    s.command = Some("npx some-mcp".into());
    let problems = validate_servers(&[s]);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("stdio") && p.contains("hosted v1")),
        "{problems:?}"
    );
}

#[test]
fn a_reserved_server_name_is_refused_in_any_case() {
    for name in ["opencompany", "OpenCompany", " gitbooks ", "GITBOOKS"] {
        let problems = validate_one("mcp server", &server(name, "https://mcp.example/mcp"));
        assert_eq!(problems.len(), 1, "`{name}` must be refused: {problems:?}");
        assert!(problems[0].contains("reserved"), "{problems:?}");
    }
    assert!(
        validate_one(
            "mcp server",
            &server("opencompany-crm", "https://mcp.example/mcp")
        )
        .is_empty()
    );
}

#[test]
fn a_manifest_declaring_a_reserved_server_name_fails_validation() {
    let problems = validate_servers(&[server("OpenCompany", "https://mcp.example/mcp")]);
    assert!(
        problems.iter().any(|problem| problem.contains("reserved")),
        "{problems:?}"
    );
}

#[cfg(feature = "openhuman")]
#[test]
fn the_reserved_names_are_the_servers_the_runtime_owns() {
    assert!(RESERVED_SERVER_NAMES.contains(&crate::hive::mcp_server::SERVER_SLUG));
    assert!(RESERVED_SERVER_NAMES.contains(&openhuman_core::mcp::host::GITBOOKS_SERVER_NAME));
}
