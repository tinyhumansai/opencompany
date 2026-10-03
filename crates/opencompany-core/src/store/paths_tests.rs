use super::*;

#[test]
fn slug_sanitizes_unsafe_characters() {
    assert_eq!(slug(&CompanyId::new("acme-co")), "acme-co");
    assert_eq!(slug(&CompanyId::new("a/b/../c")), "a_b_.._c");
    assert_eq!(slug(&CompanyId::new("")), "_");
}

#[test]
fn secret_filenames_are_injective() {
    let bundle = Bundle::new("/root", &CompanyId::new("acme"));
    let space = bundle.secret("mcp/acme prod/auth");
    let underscore = bundle.secret("mcp/acme_prod/auth");

    assert_ne!(space, underscore);
    assert!(space.ends_with("%k-mcp%2Facme%20prod%2Fauth"));
    assert!(underscore.ends_with("%k-mcp%2Facme%5Fprod%2Fauth"));
    assert_eq!(
        bundle.legacy_secret("mcp/acme prod/auth"),
        bundle.legacy_secret("mcp/acme_prod/auth")
    );
}

#[test]
fn secret_filenames_distinguish_letter_case() {
    let bundle = Bundle::new("/root", &CompanyId::new("acme"));

    // `validate_servers` treats `Acme` and `acme` as two distinct valid MCP
    // server names, so their credential keys must stay apart even on
    // filesystems that fold case (the macOS and Windows default). Upper-case
    // letters are percent-encoded while lower-case ones pass through, so
    // the two filenames differ byte-wise and stay distinct once a
    // case-insensitive filesystem lower-cases them.
    let upper = bundle.secret("mcp/Acme/auth");
    let lower = bundle.secret("mcp/acme/auth");
    assert_ne!(upper, lower);
    assert!(upper.ends_with("%k-mcp%2F%41cme%2Fauth"));
    assert!(lower.ends_with("%k-mcp%2Facme%2Fauth"));
}

#[test]
fn secret_filenames_do_not_end_in_a_period() {
    let bundle = Bundle::new("/root", &CompanyId::new("acme"));

    // Windows Win32 paths strip trailing periods, so `foo` and `foo.`
    // would resolve to one directory entry. The trailing `.` is encoded as
    // `%2E`, so the two keys stay apart and the filename can never end in
    // a dot for Windows to strip.
    let plain = bundle.secret("foo");
    let trailing_dot = bundle.secret("foo.");
    assert_ne!(plain, trailing_dot);
    assert!(plain.ends_with("%k-foo"));
    assert!(trailing_dot.ends_with("%k-foo%2E"));
    let name = trailing_dot.file_name().unwrap().to_string_lossy();
    assert!(!name.ends_with('.'));

    // Several trailing periods are each distinct too.
    let two = bundle.secret("foo..");
    assert_ne!(two, plain);
    assert_ne!(two, trailing_dot);

    // Interior periods are unaffected: a dot mid-key is a normal filename
    // character on Windows, only a trailing one is stripped.
    let interior = bundle.secret("foo.bar");
    assert!(interior.ends_with("%k-foo.bar"));
}

#[test]
fn canonical_filenames_are_disjoint_from_legacy_slugs() {
    let bundle = Bundle::new("/root", &CompanyId::new("acme"));

    // `key-` is itself a valid legacy slug, so the old `key-` canonical
    // prefix let a canonical file for `foo` be read (or deleted) as the
    // legacy file of `key-foo`. `%` cannot be emitted by `slug`, so the two
    // namespaces are structurally disjoint.
    let canonical = bundle.secret("foo");
    let legacy_of_prefix_key = bundle.legacy_secret("key-foo");
    assert_ne!(canonical, legacy_of_prefix_key);
    assert!(canonical.ends_with("%k-foo"));
    assert!(legacy_of_prefix_key.ends_with("key-foo"));

    // No legacy slug can start with `%`, so the canonical namespace can
    // never be entered through the legacy fallback.
    for key in [
        "foo",
        "key-foo",
        "key_foo",
        "a/b/../c",
        "mcp/acme prod/auth",
    ] {
        let legacy = bundle.legacy_secret(key);
        let file_name = legacy.file_name().unwrap().to_string_lossy();
        assert!(
            !file_name.starts_with('%'),
            "legacy slug of {key:?} is {file_name:?}, which starts with %"
        );
    }
}

#[test]
fn secret_filenames_are_bounded() {
    let bundle = Bundle::new("/root", &CompanyId::new("acme"));

    // An emoji MCP server name percent-encodes to ~3 bytes per UTF-8 byte;
    // the filename must stay inside the filesystem component limit whatever
    // the key, or `set` fails with ENAMETOOLONG on a 255-byte filesystem.
    let emoji_name = "🎯".repeat(40); // 40 emoji = 160 UTF-8 bytes
    let emoji = bundle.secret(&format!("mcp/{emoji_name}/auth"));
    let emoji_file = emoji.file_name().unwrap().to_str().unwrap();
    assert!(
        emoji_file.len() < 255,
        "emoji key produced a {} byte filename",
        emoji_file.len()
    );
    assert!(
        emoji_file.starts_with("%l-"),
        "expected truncated form, got {emoji_file}"
    );

    // Long ASCII keys (no percent-encoding) stay bounded too.
    let ascii = bundle.secret(&"a".repeat(400));
    let ascii_file = ascii.file_name().unwrap().to_str().unwrap();
    assert!(
        ascii_file.len() < 255,
        "long ASCII key produced a {} byte filename",
        ascii_file.len()
    );

    // Distinct long keys sharing a prefix still get distinct filenames.
    let a = bundle.secret(&format!("{}{}", "a".repeat(300), "X"));
    let b = bundle.secret(&format!("{}{}", "a".repeat(300), "Y"));
    assert_ne!(a, b);
}

#[test]
fn empty_secret_key_is_distinct() {
    let bundle = Bundle::new("/root", &CompanyId::new("acme"));
    assert_ne!(bundle.secret(""), bundle.secret("a"));
    assert!(bundle.secret("").ends_with("%k-"));
}

#[test]
fn bundle_paths_nest_under_company_slug() {
    let bundle = Bundle::new("/root", &CompanyId::new("acme"));
    assert!(bundle.dir().ends_with("companies/acme"));
    assert!(
        bundle
            .events_jsonl()
            .ends_with("companies/acme/events.jsonl")
    );
    assert!(bundle.traces_jsonl().ends_with("memory/traces.jsonl"));
    assert!(
        bundle
            .context_index_jsonl()
            .ends_with("context/index.jsonl")
    );
}

#[test]
fn keys_paths_nest_and_are_excluded_from_exports() {
    let bundle = Bundle::new("/root", &CompanyId::new("acme"));
    assert!(bundle.keys_dir().ends_with("companies/acme/keys"));
    assert!(Bundle::export_excludes().contains(&"keys"));
    assert!(Bundle::export_excludes().contains(&"secrets"));
}

/// `resolve_home_from` with only the values a case cares about.
fn resolve(flag: Option<&str>, data_dir: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    resolve_home_from(
        flag.map(PathBuf::from),
        data_dir.map(OsString::from),
        None,
        home.map(OsString::from),
        None,
    )
}

#[test]
fn the_flag_outranks_the_data_dir_variable() {
    // An explicit --home is never overridden by the environment.
    assert_eq!(
        resolve(Some("/flag"), Some("/env"), Some("/home/u")).unwrap(),
        PathBuf::from("/flag")
    );
}

#[test]
fn the_data_dir_variable_outranks_the_default() {
    // The bug this file exists to fix: OPENCOMPANY_DATA_DIR is read, and
    // used verbatim so it matches the `--home "$OPENCOMPANY_DATA_DIR"` a
    // hosted tenant's entrypoint passes.
    assert_eq!(
        resolve(None, Some("/data"), Some("/home/u")).unwrap(),
        PathBuf::from("/data")
    );
    // Verbatim means bundles land at <root>/companies/<slug>, i.e. exactly
    // DataLayout::companies_dir() — one root for the whole instance.
    let bundle = Bundle::new(
        resolve(None, Some("/data"), Some("/home/u")).unwrap(),
        &CompanyId::new("acme"),
    );
    assert_eq!(bundle.dir(), Path::new("/data/companies/acme"));
}

#[test]
fn the_default_resolves_to_the_workspace_root() {
    // The default no longer appends a `companies` leaf on top of the one
    // `Bundle::new` adds, so a default local install has the same
    // single-root shape as a hosted tenant. Existing doubled installs are
    // moved up by `store::migrate::migrate_legacy_nest` rather than orphaned.
    assert_eq!(
        resolve(None, None, Some("/home/u")).unwrap(),
        PathBuf::from("/home/u/.opencompany")
    );
    let bundle = Bundle::new(
        resolve(None, None, Some("/home/u")).unwrap(),
        &CompanyId::new("acme"),
    );
    assert_eq!(
        bundle.dir(),
        Path::new("/home/u/.opencompany/companies/acme")
    );
    // No $HOME keeps the relative fallback.
    assert_eq!(
        resolve(None, None, None).unwrap(),
        PathBuf::from(".opencompany")
    );
}

#[test]
fn every_branch_resolves_to_the_same_shape() {
    // Flag, variable, and default now agree: the home is the workspace root
    // and bundles hang off `<root>/companies/<slug>` in all three.
    let roots = [
        resolve(Some("/root"), None, None).unwrap(),
        resolve(None, Some("/root"), None).unwrap(),
        resolve(None, None, Some("/root")).unwrap(),
    ];
    assert_eq!(roots[0], roots[1]);
    assert_eq!(roots[2], PathBuf::from("/root/.opencompany"));
    for root in roots {
        let bundle = Bundle::new(root.clone(), &CompanyId::new("acme"));
        assert_eq!(bundle.dir(), root.join("companies").join("acme"));
    }
}

#[test]
fn an_empty_data_dir_counts_as_unset() {
    // Empty would otherwise root the instance at the working directory.
    assert_eq!(
        resolve(None, Some(""), Some("/home/u")).unwrap(),
        PathBuf::from("/home/u/.opencompany")
    );
    assert_eq!(
        resolve(None, Some(""), Some("")).unwrap(),
        PathBuf::from(".opencompany")
    );
}

/// Windows has no `HOME`, and the fallback below it is a RELATIVE path.
///
/// A double-clicked desktop app resolves a relative root against whatever
/// working directory the launcher gave it — plausibly `C:\Program Files`,
/// plausibly unwritable, and plausibly *different* between launches. That
/// last one is the dangerous part: two runs would quietly use two stores.
#[test]
fn a_windows_profile_stands_in_for_a_missing_home() {
    let win = |home: Option<&str>, profile: Option<&str>| {
        resolve_home_from(
            None,
            None,
            None,
            home.map(OsString::from),
            profile.map(OsString::from),
        )
        .unwrap()
    };

    assert_eq!(
        win(None, Some("C:\\Users\\ada")),
        PathBuf::from("C:\\Users\\ada").join(".opencompany")
    );
    // `HOME` wins where both are set: git-bash and MSYS set both, and a
    // user who has `HOME` set means it.
    assert_eq!(
        win(Some("/home/ada"), Some("C:\\Users\\ada")),
        PathBuf::from("/home/ada/.opencompany")
    );
    // An empty profile is not a location.
    assert_eq!(win(None, Some("")), PathBuf::from(".opencompany"));
    // Neither: the documented relative default, unchanged.
    assert_eq!(win(None, None), PathBuf::from(".opencompany"));
}

#[test]
fn the_removed_home_variable_fails_loudly() {
    let err = resolve_home_from(
        None,
        None,
        Some(OsString::from("/custom/home")),
        Some(OsString::from("/home/u")),
        None,
    )
    .expect_err("OPENCOMPANY_HOME must not be silently ignored");
    let message = err.to_string();
    assert!(message.contains(REMOVED_HOME_ENV), "{message}");
    assert!(
        message.contains(DATA_DIR_ENV),
        "names the real knob: {message}"
    );

    // Loud even alongside an explicit --home, so the mistaken belief that
    // the variable does something is always corrected.
    assert!(
        resolve_home_from(
            Some(PathBuf::from("/flag")),
            None,
            Some(OsString::from("/custom/home")),
            None,
            None,
        )
        .is_err()
    );

    // An empty value is not "set" and stays silent.
    assert!(resolve_home_from(None, None, Some(OsString::new()), None, None).is_ok());
}

#[test]
fn aligned_roots_never_warn() {
    // Hosted: the entrypoint passes --home "$OPENCOMPANY_DATA_DIR", and
    // OPENCOMPANY_DATA_DIR alone lands the same way.
    assert!(
        home_divergence_warning(Path::new("/data"), Path::new("/data")).is_none(),
        "one root for the whole instance is the intended shape"
    );
    // The default local run: both the home and the data root resolve to
    // $HOME/.opencompany now, so an ordinary run is silent without needing a
    // special case for a doubled shape.
    assert!(
        home_divergence_warning(
            Path::new("/home/u/.opencompany"),
            Path::new("/home/u/.opencompany"),
        )
        .is_none()
    );
}

#[test]
fn recreating_the_old_doubled_shape_by_hand_now_warns() {
    // A deliberate consequence of dropping the default's `companies` leaf:
    // an explicit --home at the old path really does put the bundles one
    // level away from the workspace, which is exactly what this warning is
    // for. It used to be special-cased silent.
    let warning = home_divergence_warning(
        Path::new("/home/u/.opencompany/companies"),
        Path::new("/home/u/.opencompany"),
    )
    .expect("an explicit --home at the legacy path is a real divergence");
    assert!(
        warning.contains("/home/u/.opencompany/companies"),
        "{warning}"
    );
}

#[test]
fn a_split_instance_warns_with_both_roots_named() {
    // `--home` disagreeing with a set OPENCOMPANY_DATA_DIR.
    let warning = home_divergence_warning(Path::new("/flag"), Path::new("/data"))
        .expect("a disagreeing flag and data root must warn");
    assert!(warning.contains("/flag"), "{warning}");
    assert!(warning.contains("/data"), "{warning}");

    // `--home` alone, with the data root left at its default: the bundles
    // separate but the shared workspace does not, which is the half-working
    // isolation that made this bug expensive.
    let warning =
        home_divergence_warning(Path::new("/tmp/oc-a"), Path::new("/home/u/.opencompany"))
            .expect("--home alone leaves the workspace shared and must warn");
    assert!(warning.contains("/tmp/oc-a"), "{warning}");
    assert!(warning.contains("/home/u/.opencompany"), "{warning}");
}
