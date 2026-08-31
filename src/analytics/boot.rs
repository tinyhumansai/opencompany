//! Wiring analytics into a booting host: one function, called once.
//!
//! Kept out of `src/bin/opencompany.rs` so it is testable. The binary's `serve`
//! arm is ~200 lines of sequencing that nothing else runs, and a decision this
//! consequential — whether a GPL-3.0 install reports — should not only be
//! exercised by starting a real server.

use std::sync::Arc;

use crate::analytics::config::{Decision, resolve};
use crate::analytics::types::{Envelope, OpaqueId};
use crate::analytics::{DeferredTracker, Event, Tracker, mixpanel};
use crate::app::AppState;
use crate::app::config::EnvSource;
use crate::app::deployment::Deployment;

/// Chooses this process's tracker, installs it behind `handle`, and reports
/// `instance_started`.
///
/// Called **after** the host's companies are registered, for two reasons that
/// both come down to reporting what is true rather than what was configured:
///
/// * the context envelope names the cognition path, and that is a property of
///   the brain a runtime *builds* — see [`DeferredTracker`] for why the handle
///   is handed out before the tracker exists;
/// * `instance_started` carries the company count, which is not known until
///   they have been registered or adopted.
///
/// Returns the decision so the caller can say, in one line, why a host that an
/// operator expected to report is not reporting.
pub fn install(state: &AppState, handle: &DeferredTracker, env: &dyn EnvSource) -> Decision {
    let deployment = Deployment::from_env(env);
    let decision = resolve(deployment, env);

    // Identity, in the order #1739's decision 2 sets out: the tenant slug
    // (hashed) when the platform named one, else this host's own random
    // instance id. Never the company name, and never anything derived from the
    // hostname or the bind address — `crate::app::instance` argues that at
    // length for the same id on the same grounds.
    let id = identify(state, env);

    // The cognition seam the rest of the tree already uses, rather than a second
    // derivation of "which brain is this host on?" beside the code that picks
    // one.
    //
    // **This is a host-level label, not a per-company one, and the difference is
    // real.** Inference is configured per company, so `serve --company a
    // --company b` with one configured and one on the echo fallback gives two
    // cognition paths and one envelope — the first registered runtime answers
    // for both. `Envelope::set_cognition` then makes the label the *most
    // recently observed* rather than the first, which is right for the case it
    // exists for (a host whose one company is provisioned or rebuilt after
    // boot) and no more correct than the first for a genuinely mixed host.
    //
    // Making it per-company means moving cognition off the envelope's
    // super-properties and onto `turn_finished` and `turn_metered` themselves,
    // which changes the payload shape #1739 shipped — an analytics-contract
    // decision rather than a defect fix, and one `instance_started` (which has
    // no company) does not fit. Raised on PR #1751 and left for its own change.
    //
    // A host with no companies yet reports the default descriptor, which
    // honestly says `custom`/`unknown` until `observe_cognition` corrects it.
    let cognition = state
        .registry()
        .list()
        .first()
        .and_then(|id| state.registry().get(id))
        .map(|runtime| runtime.cognition())
        .unwrap_or_default();

    let envelope = Envelope::new(id, deployment, cognition);
    let tracker: Arc<dyn Tracker> = mixpanel::build(&decision, envelope);
    handle.install(tracker);

    handle.track(Event::InstanceStarted {
        companies: state.registry().list().len() as u64,
        storage: state.storage_kind().as_str(),
        setup_complete: state.setup_complete() || !state.registry().is_empty(),
    });

    decision
}

/// Chooses the opaque id this host's events are attributed to.
///
/// Split out of [`install`] so the choice can be asserted directly. It was
/// inline, and the test that covered it recomputed the expected id itself and
/// compared the two — which passes whatever `install` actually does, and did:
/// a deliberate mutation making the keyless path fall back to a baked-in salt
/// went undetected. A decision this consequential has to be observable.
///
/// A tenant slug is identified by keyed digest, and **only** when the platform
/// configured a key. There is deliberately no unkeyed fallback: a plain hash of
/// a slug is not an opaque id, because the slug is usually the customer's brand
/// and a few thousand guesses invert it, and a salt compiled into a GPL-3.0
/// binary is one every reader of the source already has. Without a key the host
/// identifies *itself*, by the random instance id that names nobody's customer
/// — see [`OpaqueId`] for the full argument.
pub(crate) fn identify(state: &AppState, env: &dyn EnvSource) -> OpaqueId {
    match (
        state.config().tenant_namespace.as_deref(),
        crate::analytics::config::tenant_id_key(env),
    ) {
        (Some(tenant), Some(key)) => OpaqueId::tenant(crate::app::canonical_tenant(tenant), &key),
        _ => OpaqueId::instance(state.instance_id()),
    }
}

/// The one line a boot log carries about analytics.
///
/// Said out loud on purpose. Silence is the correct default, but a *silent*
/// default is how an operator spends an afternoon on a tenant that was never
/// going to report — and, in the other direction, a hosted tenant's operator is
/// entitled to see in their own logs that reporting is on.
///
/// It reports what the process will actually **do**, not what was configured,
/// and those differ in exactly one case: a build compiled without the
/// `analytics` feature resolves [`Decision::Report`] and then gets a
/// [`NullTracker`](crate::analytics::NullTracker) from
/// [`mixpanel::build`](crate::analytics::mixpanel::build), because there is no
/// transport in it to hand back. Saying "reporting to …" there is the exact
/// opposite of the truth, and the `mixpanel::build` line that explains it is a
/// `tracing::info!` the CLI's default `EnvFilter` swallows — which is why every
/// other boot line here is a `println!`. So the build is named on this line
/// instead.
pub fn describe(decision: &Decision) -> String {
    match decision {
        Decision::Silent(reason) => {
            format!("analytics: off ({})", reason.as_str())
        }
        // The endpoint, never the token — in either arm.
        Decision::Report { endpoint, .. }
            if crate::analytics::BuildFlags::of_this_build().analytics =>
        {
            format!("analytics: reporting to {}", loggable_endpoint(endpoint))
        }
        Decision::Report { endpoint, .. } => format!(
            "analytics: off (reporting to {} was configured, but this build was \
             compiled without the `analytics` feature)",
            loggable_endpoint(endpoint)
        ),
    }
}

/// The collector URL with anything credential-shaped removed.
///
/// `OPENCOMPANY_ANALYTICS_ENDPOINT` exists so a deployment can front Mixpanel
/// with its own proxy, and an authenticated proxy carries its key in exactly the
/// two places a URL can hold one: userinfo (`https://user:pass@host/track`) and
/// the query string (`https://host/track?key=…`). Printing the raw value writes
/// that secret verbatim into container logs, which the [`ProjectToken`]
/// redaction does nothing about — it guards a different string.
///
/// A third place, which the two above do not reach: an opaque **path segment**,
/// as in `https://collector.example/ingest/<token>`, which is how a signed-URL
/// collector is usually configured. So the path is kept only as far as its first
/// segment — enough to name the route, which is what makes the line useful — and
/// the rest is elided rather than inspected, because "does this look like a
/// secret?" is not a question worth answering heuristically. The default
/// endpoint has a single segment (`/track`) and is unaffected.
///
/// So only the scheme, host and leading path segment are logged, which is all an
/// operator needs to answer "where is this going?". When something was removed
/// the line says so, because a silently shortened URL is its own hour of
/// confusion.
///
/// `pub(crate)` because the transport logs the same destination when a send
/// fails (`crate::analytics::mixpanel`). One helper, deliberately: a second
/// redaction of the same string is a second thing to keep correct, and the two
/// diverge the first time only one of them learns about a new place a URL can
/// hold a secret.
///
/// [`ProjectToken`]: crate::analytics::config::ProjectToken
pub(crate) fn loggable_endpoint(raw: &str) -> String {
    // Query and fragment first: `?key=…` is at least as common as userinfo.
    let trimmed = raw.split(['?', '#']).next().unwrap_or(raw);

    // Then userinfo, and only within the authority — a path may legitimately
    // contain `@`, and truncating there would report the wrong destination.
    let cleaned = match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = match rest.split_once('/') {
                Some((authority, path)) => (authority, Some(path)),
                None => (rest, None),
            };
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_userinfo, host)| host);
            match path {
                // Only the **first** path segment. A proxy can sign a request
                // with an opaque path segment too — `https://collector/ingest/<token>`
                // is how a signed-URL collector is usually configured — and that
                // segment is a credential in a place neither the userinfo nor
                // the query strip reaches. The first segment names the route,
                // which is what makes the line useful; anything after it is
                // elided rather than guessed at, because "does this look like a
                // secret?" is not a question worth answering heuristically.
                //
                // The default endpoint has one segment (`/track`), so the
                // ordinary line is unchanged.
                Some(path) => {
                    let mut segments = path.split('/');
                    let first = segments.next().unwrap_or("");
                    if segments.any(|segment| !segment.is_empty()) {
                        format!("{scheme}://{host}/{first}/…")
                    } else {
                        format!("{scheme}://{host}/{path}")
                    }
                }
                None => format!("{scheme}://{host}"),
            }
        }
        None => trimmed.to_string(),
    };

    if cleaned == raw {
        cleaned
    } else {
        format!("{cleaned} (credentials redacted)")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::analytics::config::{ENABLE_ENV, Silence, TOKEN_ENV};
    use crate::app::config::MapEnv;
    use crate::app::deployment::DEPLOYMENT_ENV;
    use crate::{AppConfig, AppState};

    fn state() -> (AppState, tempfile::TempDir) {
        let home = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(AppConfig::default()).with_home(home.path());
        (state, home)
    }

    /// **The default posture, asserted end to end at the boot seam.** A host
    /// that says nothing installs a tracker that sends nothing, even with a
    /// token sitting in its environment.
    #[test]
    fn an_undeclared_host_installs_silence() {
        let (state, _home) = state();
        let handle = DeferredTracker::new();
        let decision = install(
            &state,
            &handle,
            &MapEnv::new([(TOKEN_ENV, "not-a-real-token")]),
        );
        assert_eq!(decision, Decision::Silent(Silence::NotHosted));
        assert_eq!(
            describe(&decision),
            "analytics: off (not a hosted tenant and no explicit opt-in)"
        );
    }

    /// A hosted tenant resolves to reporting. In a build without the `analytics`
    /// feature the installed tracker is still a no-op — the decision is the same
    /// either way, which is what this pins.
    #[test]
    fn a_hosted_tenant_resolves_to_reporting() {
        let (state, _home) = state();
        let handle = DeferredTracker::new();
        let decision = install(
            &state,
            &handle,
            &MapEnv::new([
                (DEPLOYMENT_ENV, "hosted-tenant"),
                (TOKEN_ENV, "not-a-real-token"),
            ]),
        );
        assert!(decision.reports(), "{decision:?}");
        assert!(
            !describe(&decision).contains("not-a-real-token"),
            "the boot line must not carry the token: {}",
            describe(&decision)
        );
    }

    /// **The boot line reports behaviour, not configuration.** A build with no
    /// `analytics` feature installs a `NullTracker` for a reporting decision, so
    /// the line must not claim it is reporting; a build with the feature must.
    /// The two halves are asserted from one `cfg!`, so the default lane and the
    /// scoped `analytics` lane each exercise their own branch and neither can
    /// pass by ignoring the build.
    #[test]
    fn the_boot_line_says_when_the_build_has_no_transport() {
        let decision = Decision::Report {
            endpoint: "https://collector.invalid/track".to_string(),
            token: crate::analytics::config::ProjectToken::new("not-a-real-token"),
        };
        let line = describe(&decision);
        assert!(!line.contains("not-a-real-token"), "{line}");

        if cfg!(feature = "analytics") {
            assert_eq!(
                line,
                "analytics: reporting to https://collector.invalid/track"
            );
        } else {
            assert!(
                line.starts_with("analytics: off ("),
                "a build with no transport must not read as reporting: {line}"
            );
            assert!(line.contains("without the `analytics` feature"), "{line}");
            assert!(
                line.contains("https://collector.invalid/track"),
                "the configured endpoint is still named, so the operator can see \
                 what was intended: {line}"
            );
        }
    }

    /// **A credential in the endpoint must not reach a log line.**
    ///
    /// `OPENCOMPANY_ANALYTICS_ENDPOINT` is there so a deployment can front
    /// Mixpanel with its own proxy, and an authenticated proxy carries its key
    /// in userinfo or in the query string. `ProjectToken`'s redaction guards a
    /// different string entirely and does nothing here.
    ///
    /// Asserted case-insensitively: a redaction that merely lowercased the
    /// secret would still have leaked it, and an exact-case search would read
    /// that as clean.
    #[test]
    fn a_credential_in_the_endpoint_never_reaches_the_boot_line() {
        for raw in [
            "https://someone:NotARealCollectorKey@collector.invalid/track",
            "https://collector.invalid/track?key=NotARealCollectorKey",
            "https://collector.invalid/track#NotARealCollectorKey",
            "https://someone:NotARealCollectorKey@collector.invalid/t?k=NotARealCollectorKey",
            // The third place a URL can carry one, which userinfo and query
            // stripping both miss: a signed path segment.
            "https://collector.invalid/ingest/NotARealCollectorKey",
            "https://collector.invalid/v1/ingest/NotARealCollectorKey/track",
        ] {
            let line = describe(&Decision::Report {
                endpoint: raw.to_string(),
                token: crate::analytics::config::ProjectToken::new("not-a-real-token"),
            });
            assert!(
                !line
                    .to_ascii_lowercase()
                    .contains(&SECRET.to_ascii_lowercase()),
                "the boot line leaked the endpoint credential in {raw:?}: {line}"
            );
            assert!(
                line.contains("collector.invalid"),
                "the destination is still named, or the line is useless: {line}"
            );
            assert!(
                line.contains("credentials redacted"),
                "a shortened URL must say it was shortened: {line}"
            );
        }
    }

    /// A credential-shaped value that stands in for a collector proxy's key.
    /// Mixed case on purpose: see the self-check below.
    const SECRET: &str = "NotARealCollectorKey";

    /// The self-check for the assertion above. A leak guard that cannot find
    /// the needle in an **unredacted** value proves nothing about the redacted
    /// one — the needle may simply never have been there, or the comparison may
    /// be case-sensitive against a value something lowercased on the way
    /// through. Both have happened on this PR.
    #[test]
    fn the_leak_assertion_would_catch_an_unredacted_endpoint() {
        let unredacted = format!("https://collector.invalid/track?key={SECRET}");
        assert!(
            unredacted
                .to_ascii_lowercase()
                .contains(&SECRET.to_ascii_lowercase()),
            "the needle must be findable before redaction, or the guard is vacuous"
        );
        assert!(
            unredacted
                .to_ascii_lowercase()
                .contains(&SECRET.to_ascii_uppercase().to_ascii_lowercase()),
            "and findable whatever case it comes back in"
        );
    }

    /// An ordinary endpoint is printed unchanged and says nothing about
    /// redaction. The control: without it, "redact everything" would pass.
    #[test]
    fn an_ordinary_endpoint_is_printed_unchanged() {
        let line = describe(&Decision::Report {
            endpoint: crate::analytics::config::DEFAULT_ENDPOINT.to_string(),
            token: crate::analytics::config::ProjectToken::new("not-a-real-token"),
        });
        assert!(
            line.contains(crate::analytics::config::DEFAULT_ENDPOINT),
            "{line}"
        );
        assert!(!line.contains("credentials redacted"), "{line}");
    }

    /// An unreadable switch says so at boot, rather than looking like a
    /// deliberate opt-out or like a working opt-in.
    #[test]
    fn an_unreadable_switch_says_so() {
        let (state, _home) = state();
        let handle = DeferredTracker::new();
        let decision = install(
            &state,
            &handle,
            &MapEnv::new([
                (DEPLOYMENT_ENV, "hosted-tenant"),
                (ENABLE_ENV, "of"),
                (TOKEN_ENV, "not-a-real-token"),
            ]),
        );
        assert_eq!(decision, Decision::Silent(Silence::Unreadable));
        assert!(describe(&decision).contains("not recognised"));
    }

    /// A tenant state, for the identity tests below.
    fn tenant_state(tenant: &str) -> (AppState, tempfile::TempDir) {
        let home = tempfile::tempdir().expect("tempdir");
        let state = AppState::new(AppConfig {
            tenant_namespace: Some(tenant.into()),
            ..AppConfig::default()
        })
        .with_home(home.path());
        (state, home)
    }

    /// **A hosted tenant with no identity key is known by its instance id, not
    /// by a digest of its slug.**
    ///
    /// The dangerous fallback is the other one. An unkeyed `SHA-256(slug)` is
    /// not an opaque identity: a slug is usually the customer's brand, so
    /// whoever holds the digests can enumerate candidates and read the customer
    /// straight back out. When the platform has not supplied a key there is
    /// nothing private to derive, so the host names *itself* — 128 random bits
    /// that identify nobody's customer — rather than naming its customer badly.
    ///
    /// Asserted on what `identify` actually returns. An earlier version of this
    /// test recomputed the expected id itself and compared the two, which is a
    /// tautology: a mutation replacing the keyless path with a baked-in salt
    /// passed it.
    #[test]
    fn a_tenant_without_an_identity_key_falls_back_to_the_instance_id() {
        let (state, _home) = tenant_state("acmecorp-holdings");
        let chosen = identify(
            &state,
            &MapEnv::new([
                (DEPLOYMENT_ENV, "hosted-tenant"),
                (TOKEN_ENV, "not-a-real-token"),
            ]),
        );

        assert_eq!(
            chosen.as_str(),
            OpaqueId::instance(state.instance_id()).as_str(),
            "a keyless tenant must be known by its own random instance id"
        );
        assert!(
            chosen.as_str().starts_with("i_"),
            "and never by anything in the tenant id space: {chosen:?}"
        );
    }

    /// And with a key the tenant *is* identified as a tenant — the control,
    /// without which "it always uses the instance id now" would pass the test
    /// above just as well.
    #[test]
    fn a_tenant_with_an_identity_key_is_identified_as_one() {
        let (state, _home) = tenant_state("acmecorp-holdings");
        let chosen = identify(
            &state,
            &MapEnv::new([
                (DEPLOYMENT_ENV, "hosted-tenant"),
                (TOKEN_ENV, "not-a-real-token"),
                (crate::analytics::config::ID_KEY_ENV, "not-a-real-id-key"),
            ]),
        );

        let key = crate::analytics::types::TenantIdKey::new("not-a-real-id-key")
            .expect("a non-blank key");
        assert_eq!(
            chosen.as_str(),
            OpaqueId::tenant("acmecorp-holdings", &key).as_str(),
            "a keyed tenant is identified by its keyed digest"
        );
        assert!(chosen.as_str().starts_with("t_"), "{chosen:?}");
        assert_ne!(
            chosen.as_str(),
            OpaqueId::instance(state.instance_id()).as_str(),
            "the two id spaces must not collide"
        );
    }

    /// A blank key is not a key: it must send the tenant to the instance id
    /// rather than deriving under a key everybody can guess.
    #[test]
    fn a_blank_identity_key_does_not_identify_a_tenant() {
        let (state, _home) = tenant_state("acmecorp-holdings");
        for blank in ["", "   ", "\n"] {
            let chosen = identify(
                &state,
                &MapEnv::new([
                    (DEPLOYMENT_ENV, "hosted-tenant"),
                    (TOKEN_ENV, "not-a-real-token"),
                    (crate::analytics::config::ID_KEY_ENV, blank),
                ]),
            );
            assert_eq!(
                chosen.as_str(),
                OpaqueId::instance(state.instance_id()).as_str(),
                "a key of {blank:?} must read as absent"
            );
        }
    }

    /// The instance id is what a host with no tenant namespace is known by, and
    /// it is prefixed so it can never be confused with a tenant digest.
    #[test]
    fn an_untenanted_host_is_known_by_its_instance_id() {
        let (state, _home) = state();
        let expected = OpaqueId::instance(state.instance_id());
        assert!(expected.as_str().starts_with("i_"));
        assert!(expected.as_str().contains(state.instance_id()));
    }

    #[test]
    fn an_opted_out_host_says_why() {
        let (state, _home) = state();
        let handle = DeferredTracker::new();
        let decision = install(
            &state,
            &handle,
            &MapEnv::new([
                (DEPLOYMENT_ENV, "hosted-tenant"),
                (ENABLE_ENV, "off"),
                (TOKEN_ENV, "not-a-real-token"),
            ]),
        );
        assert_eq!(decision, Decision::Silent(Silence::OptedOut));
        assert!(describe(&decision).contains("operator opted out"));
    }
}
