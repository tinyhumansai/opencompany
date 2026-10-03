//! What `serve` wires that the desktop does not.
//!
//! Two entry points build a host: `serve`, in `src/bin/opencompany.rs`, and the
//! embedded host the desktop shell starts, in
//! `crates/opencompany-app/src/embedded.rs`. Each assembles its `AppConfig`,
//! its `AppState` and its per-company `RuntimeBuilder` from a hand-written list
//! of calls, and nothing compared the two lists. Every divergence found so far
//! was found by a person hitting it in the console — an `api_url` pinned to the
//! production constant on a host pointed at staging, an Account page reporting
//! that the host belonged to no ecosystem, a `config.toml` the shell had
//! written and then ignored.
//!
//! So this compares them. It reads the four source files, extracts the wiring
//! each side performs, and asserts the difference is exactly
//! [`DELIBERATE_DIFFERENCES`]. Restoring a gap means deleting its row; a
//! difference that is genuinely correct means writing down *why*. What it will
//! not allow is a fifth gap arriving the way the first four did — silently,
//! with the failure surfacing months later in somebody's console.
//!
//! ## Why it reads source text
//!
//! The comparison it would rather make — build both hosts, diff the resulting
//! states — cannot be made. `serve`'s wiring lives inside a match arm in `main`
//! that resolves the process environment, locks a data root, opens storage
//! backends and binds a listener; there is no seam to call. The desktop's lives
//! in a crate deliberately excluded from this workspace. Source text is what
//! both have in common, and the precedent is `tests/auth_matrix.rs`, which
//! scans `src/server` for route literals on the same reasoning.
//!
//! ## What it does not see
//!
//! Presence, not value. Both hosts name `bind` in their `AppConfig` literal and
//! they name it differently on purpose — `127.0.0.1:0` against a configurable
//! address — and that difference is argued where it is made, in `embedded.rs`'s
//! module docs, not here. A field both sides set to different values is not a
//! gap; a field one side leaves to the compiled-in default is.
//!
//! Builder wiring named `with_*`, not every method. `skip_activation_gate` is
//! wired by `serve` and not by the desktop, and this will not report it. The
//! convention is near-universal and deriving the universe from it is what makes
//! a *new* builder method enter the comparison for free; a method that opts out
//! of the convention opts out of the check.
//!
//! The scan is deliberately over-inclusive on the `serve` side: it reads the
//! whole binary rather than trying to delimit one match arm. A call that
//! belongs to some other subcommand is therefore attributed to `serve`, which
//! can only ever demand *more* of the desktop than is owed — a failure that
//! gets classified once into the table below, never a gap that slips past.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Wiring `serve` performs that the desktop deliberately does not, and why.
///
/// A row is a promise that somebody looked. Two kinds live here and they are
/// not the same: a difference that is *correct* (a loopback host must not
/// advertise a routable base URL) and one that is merely *not yet decided*,
/// which names the issue tracking it rather than inventing a justification.
const DELIBERATE_DIFFERENCES: &[(&str, &str)] = &[
    // ---- Correct: the desktop is one machine, one person, one loopback port.
    (
        "config.public_url",
        "a loopback host must not advertise a routable base URL",
    ),
    (
        "config.instance_name",
        "the desktop names its instances in its own roster (local.rs), which is \
         what the console shows; OPENCOMPANY_INSTANCE_NAME is the server answer \
         to the same need",
    ),
    (
        "config.openhuman_root",
        "a `serve --openhuman-root` flag, recorded for /spec and read by nothing",
    ),
    (
        "config.tenant_namespace",
        "shared-single-DB tenant namespacing; the desktop serves one install",
    ),
    (
        "config.admin_email",
        "OPENCOMPANY_ADMIN_EMAIL is injected by the provisioning platform; \
         there is no platform provisioning a desktop",
    ),
    (
        "config.sso_secret",
        "OPENCOMPANY_SSO_SECRET is the platform's shared SSO signing secret, \
         injected at provision so a hosted tenant can verify the auto-login \
         tokens the dashboard mints; a desktop host is opened by its own operator \
         with no platform issuing tokens, so it honors no SSO surface — the \
         redeem route 404s without the secret",
    ),
    (
        "state.with_quota",
        "caps on provisioned companies; the desktop exposes no provisioning API",
    ),
    (
        "state.with_platform_auth",
        "multi-tenant machine credentials; a desktop host has no tenants",
    ),
    (
        "state.with_config_root",
        "a no-op here: config_root() falls back to home(), and prepare_instance \
         resolves one root for both. serve needs it only because --home can \
         point the bundles somewhere other than the data dir",
    ),
    (
        "state.with_cors",
        "an empty allowlist leaves CORS off, which is what a loopback host \
         serving its own webview wants; reading an allowlist from the \
         environment here would only ever widen it",
    ),
    (
        "state.with_skills_root",
        "serve derives it from the `skills/` directory beside a checkout's \
         `companies/`; a packaged install has neither, and pointing this at a \
         fabricated path would be worse than serving no registry",
    ),
    (
        "builder.with_seed_dir",
        "seeds a company's workspace tree from `companies/<name>` in a \
         checkout. Desktop presets are compiled-in `&'static DesktopPreset` \
         values; there is no such directory to name",
    ),
    (
        "builder.with_bootstrap_admin",
        "pairs with config.admin_email — a platform-injected standing invite",
    ),
    (
        "builder.with_mail",
        "the injected per-tenant mailbox rides the `smtp` feature, which the \
         desktop does not compile",
    ),
    // ---- Deferred: decided against taking in this change, tracked elsewhere.
    (
        "state.with_setup_complete",
        "deferred, tracked in #2319 — the stamp is never read back, so a \
         relaunched desktop host authorizes host-level setup writes anonymously",
    ),
    (
        "state.with_stores",
        "deferred, tracked in #2320 — the desktop compiles `sqlite` and can \
         never select it",
    ),
    (
        "state.with_storage_kind",
        "deferred, tracked in #2320 — pairs with state.with_stores",
    ),
    (
        "builder.with_storage_kind",
        "deferred, tracked in #2320 — the default `fs` is the refusing \
         direction for the repository-credential gates, so absence is currently \
         the safe answer rather than a live hole",
    ),
    (
        "state.with_memory_overlay",
        "deferred, tracked in #2320 — OPENCOMPANY_MEMORY and the `[memory]` \
         section reach no desktop host",
    ),
    (
        "builder.with_memory_overlay",
        "deferred, tracked in #2320 — pairs with state.with_memory_overlay",
    ),
    (
        "builder.with_memory_overlay_cleared",
        "deferred, tracked in #2320 — the clear-on-rebuild half of the same seam",
    ),
    (
        "builder.with_task_seeding",
        "deferred, tracked in #2321 — desktop boards open with no baseline \
         setup cards. Turning it on adds cards to every board, which is a \
         product change rather than restored wiring",
    ),
    (
        "state.with_webhook",
        "OPENCOMPANY_WEBHOOK_URL reaches no desktop host, which offers no \
         outbound-webhook surface to configure one from",
    ),
    (
        "state.with_connections",
        "the desktop compiles neither `dns` nor `smtp`, so the injected seams \
         would carry only the OPENCOMPANY_MAIL_* credentials, and no desktop \
         surface sets those",
    ),
];

/// `serve`'s host construction: the whole binary, minus its test module.
fn serve_source() -> String {
    strip_tests(&read(&core_root().join("src/bin/opencompany.rs")))
}

/// The embedded host the desktop shell starts.
fn desktop_host_source() -> String {
    strip_tests(&read(
        &core_root().join("../opencompany-app/src/embedded.rs"),
    ))
}

fn core_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()))
}

/// Drops everything from the first top-level `#[cfg(test)]` on, so a fixture
/// that builds an `AppState` is never mistaken for production wiring.
fn strip_tests(source: &str) -> String {
    match source.find("\n#[cfg(test)]") {
        Some(at) => source[..at].to_string(),
        None => source.to_string(),
    }
}

/// The body of a top-level `fn name(...)`, to its closing brace in column zero.
fn function_body(source: &str, name: &str) -> String {
    let needle = format!("\nfn {name}(");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("`fn {name}` must exist — this test tracks it by name"));
    let rest = &source[start + 1..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("`fn {name}` must close with a brace in column zero"));
    let body = rest[..end].to_string();
    assert!(
        body.len() > 200,
        "`fn {name}` extracted as {} bytes, which is not a builder assembly — \
         the delimiters moved",
        body.len()
    );
    body
}

/// A side's whole per-company assembly: its builder function, plus every
/// `fn attach_*` in the same file.
///
/// The indirection is load-bearing rather than incidental. Both sides hang
/// their feature-gated wiring off small `attach_*` helpers — the OpenHuman
/// transport, the hub feedback client — and each is a `#[cfg]`-paired pair
/// whose first definition is the no-op stub. Reading only the builder function
/// would see neither half, and would have reported the hub feedback client as
/// wired on both sides while it was wired on neither.
fn assembly(source: &str, builder_fn: &str) -> String {
    let mut region = function_body(source, builder_fn);
    let mut from = 0;
    while let Some(at) = source[from..].find("\nfn attach_") {
        let start = from + at + 1;
        let end = source[start..]
            .find("\n}\n")
            .map(|e| start + e)
            .unwrap_or(source.len());
        region.push_str(&source[start..end]);
        from = end.max(start + 1);
    }
    region
}

/// Every `pub fn with_*` declared on a type, which is the universe of wiring
/// the comparison below can see.
///
/// Derived rather than listed: a builder method added to `AppState` or
/// `RuntimeBuilder` tomorrow enters this set on its own, so `serve` adopting it
/// and the desktop not adopting it is a failure without anyone remembering to
/// extend a constant.
fn builder_methods(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub fn with_"))
        .filter_map(|rest| rest.split('(').next())
        .filter(|name| !name.is_empty())
        .map(|name| format!("with_{name}"))
        .collect()
}

/// Which of `universe` a source calls, by `.name(` call site.
fn calls<'a>(source: &str, universe: impl IntoIterator<Item = &'a String>) -> BTreeSet<String> {
    universe
        .into_iter()
        .filter(|name| source.contains(&format!(".{name}(")))
        .cloned()
        .collect()
}

/// The fields `AppConfig` declares.
fn config_fields(types_source: &str) -> BTreeSet<String> {
    let start = types_source
        .find("pub struct AppConfig {")
        .expect("AppConfig must be declared in app/types.rs");
    let rest = &types_source[start..];
    let end = rest.find("\n}\n").expect("AppConfig must close");
    rest[..end]
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub "))
        .filter_map(|rest| rest.split(':').next())
        .filter(|name| !name.is_empty() && !name.contains(' '))
        .map(str::to_string)
        .collect()
}

/// Which fields a source wires: the ones its `AppConfig { .. }` literal names,
/// plus — when its rest-pattern is the shared resolver rather than
/// `AppConfig::default()` — everything that resolver resolves.
///
/// A field left to `..AppConfig::default()` is precisely the failure this
/// tracks: `api_url` was one, and a compiled-in production constant is what a
/// desktop pointed at staging got. Falling back to
/// [`AppConfig::resolve_host`](opencompany::AppConfig::resolve_host) is the
/// opposite — that pass reads the environment and `config.toml` for every field
/// it covers, so a field it covers is wired by deferring to it. A field it does
/// *not* cover is still a gap, which is what keeps this from becoming a
/// blanket excuse.
fn config_fields_set(
    source: &str,
    fields: &BTreeSet<String>,
    shared_pass: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut named = BTreeSet::new();
    if source.contains("..AppConfig::resolve_host(") {
        named.extend(shared_pass.iter().cloned());
    }
    for literal in literals(source, "AppConfig {") {
        for field in fields {
            if literal.lines().any(|line| {
                let line = line.trim();
                line.starts_with(&format!("{field}:")) || line == format!("{field},")
            }) {
                named.insert(field.clone());
            }
        }
    }
    named
}

/// Every `AppConfig { .. }` literal in a source, to its `..` rest-pattern.
fn literals(source: &str, opener: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = source[from..].find(opener) {
        let start = from + at + opener.len();
        let end = source[start..]
            .find("..")
            .map(|e| start + e)
            .unwrap_or(source.len());
        out.push(source[start..end].to_string());
        from = start;
    }
    out
}

/// The audit, as an assertion.
///
/// Each half of the host is compared against its own counterpart rather than a
/// union of both: an `AppConfig` field and a `RuntimeBuilder` call can carry
/// the same name (`workspace_quota` is set on the config *and* threaded to the
/// builder, and the desktop used to do the first without the second, which made
/// the config field dead), so collapsing them would let one side's coverage
/// stand in for the other's.
#[test]
fn the_desktop_wires_everything_serve_does_but_the_declared_differences() {
    let types = read(&core_root().join("src/app/types.rs"));
    let builder_source = read(&core_root().join("src/runtime/builder.rs"));
    let serve = serve_source();
    let desktop_host = desktop_host_source();
    let desktop_company = strip_tests(&read(&core_root().join("src/desktop.rs")));

    let state_methods = builder_methods(&types);
    let runtime_methods = builder_methods(&builder_source);
    let fields = config_fields(&types);
    assert!(
        state_methods.len() > 10 && runtime_methods.len() > 20 && fields.len() > 10,
        "the declaration scan found almost nothing, so it is scanning the wrong \
         thing: state={}, runtime={}, fields={}",
        state_methods.len(),
        runtime_methods.len(),
        fields.len()
    );

    let mut gaps = BTreeSet::new();
    for name in calls(&serve, &state_methods).difference(&calls(&desktop_host, &state_methods)) {
        gaps.insert(format!("state.{name}"));
    }
    // The per-company assembly each side's rebuilder also reuses, compared
    // function to function: these two are the same job written twice.
    let serve_builder = assembly(&serve, "company_builder");
    let desktop_builder = assembly(&desktop_company, "desktop_builder");
    for name in calls(&serve_builder, &runtime_methods)
        .difference(&calls(&desktop_builder, &runtime_methods))
    {
        gaps.insert(format!("builder.{name}"));
    }
    // What the shared resolver covers, read off its own returned literal rather
    // than restated here: a field added to `resolve_host` starts counting as
    // wired without this test being edited, and a field added to `AppConfig`
    // that `resolve_host` does not cover does not.
    let shared_pass: BTreeSet<String> = literals(&types, "Ok(Self {")
        .first()
        .map(|literal| {
            literal
                .lines()
                .filter_map(|line| {
                    let line = line.trim().trim_end_matches(',');
                    let name = line.split(':').next().unwrap_or_default();
                    fields.contains(name).then(|| name.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !shared_pass.is_empty(),
        "`AppConfig::resolve_host` resolves nothing, so the scan is reading the          wrong literal"
    );
    for field in config_fields_set(&serve, &fields, &shared_pass).difference(&config_fields_set(
        &desktop_host,
        &fields,
        &shared_pass,
    )) {
        gaps.insert(format!("config.{field}"));
    }

    let declared: BTreeSet<String> = DELIBERATE_DIFFERENCES
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    assert_eq!(
        declared.len(),
        DELIBERATE_DIFFERENCES.len(),
        "DELIBERATE_DIFFERENCES names the same wiring twice"
    );

    let undeclared: Vec<_> = gaps.difference(&declared).collect();
    let stale: Vec<_> = declared.difference(&gaps).collect();
    assert!(
        undeclared.is_empty(),
        "the desktop host does not wire what `serve` wires, and nothing says why: \
         {undeclared:?}. Either wire it in embedded.rs / desktop_builder, or add a \
         row to DELIBERATE_DIFFERENCES saying what makes the desktop different."
    );
    assert!(
        stale.is_empty(),
        "DELIBERATE_DIFFERENCES excuses wiring that no longer diverges: {stale:?}. \
         Delete those rows — a stale exception is how the next real gap hides."
    );
}

/// Every declared difference carries a reason somebody can act on.
#[test]
fn every_declared_difference_states_why() {
    for (name, reason) in DELIBERATE_DIFFERENCES {
        assert!(
            reason.len() > 30,
            "`{name}` is excused by {reason:?}, which does not say why"
        );
        assert!(
            !reason.starts_with("deferred") || reason.contains('#'),
            "`{name}` is deferred without naming the issue that tracks it: {reason:?}"
        );
    }
}
