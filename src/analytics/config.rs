//! The enable/disable decision: whether this process reports at all, and where.
//!
//! Kept apart from both the transport and the payload because it is the part
//! that has to be *provably* right. It is pure — an [`EnvSource`] and a
//! [`Deployment`] in, a [`Decision`] out — so every branch of it is tested in
//! the default build, with no network and no feature flag.

use crate::analytics::types::TenantIdKey;
use crate::app::config::EnvSource;
use crate::app::deployment::Deployment;

/// Operator override: `on` forces reporting, `off` forbids it.
pub const ENABLE_ENV: &str = "OPENCOMPANY_ANALYTICS";
/// The Mixpanel project token. **Configuration, never a compiled-in constant** —
/// a token baked into a public binary is a token everyone has.
pub const TOKEN_ENV: &str = "OPENCOMPANY_ANALYTICS_TOKEN";
/// Overrides the collector URL. Exists so a test can point at a local server,
/// and so a deployment can front Mixpanel with its own proxy.
pub const ENDPOINT_ENV: &str = "OPENCOMPANY_ANALYTICS_ENDPOINT";
/// The secret that makes a hosted tenant's analytics id unguessable.
///
/// **Configuration, never a compiled-in constant**, and for a sharper reason
/// than the project token: a salt baked into a GPL-3.0 binary is a salt every
/// reader of the source already has, which is no salt at all. Injected by the
/// platform that provisions tenants; never given to the collector. Absent means
/// the host identifies itself by its random instance id instead — see
/// [`TenantIdKey`](crate::analytics::types::TenantIdKey).
pub const ID_KEY_ENV: &str = "OPENCOMPANY_ANALYTICS_ID_KEY";

/// Where events go when nothing overrides it.
pub const DEFAULT_ENDPOINT: &str = "https://api.mixpanel.com/track";

/// A Mixpanel project token.
///
/// A newtype rather than a bare `String` for one reason: it must never be
/// printed, logged, or serialized by accident. It derives **neither** `Debug`
/// nor `Serialize` — the hand-written `Debug` redacts — because
/// `serde_json::to_value(&some_config)` is precisely how a credential reaches a
/// payload (issue #1741, `SecretValue`). Nothing in this module ever serializes
/// a config struct; the token is read out explicitly, once, at the moment a
/// request body is built.
#[derive(Clone, PartialEq, Eq)]
pub struct ProjectToken(String);

impl ProjectToken {
    /// Wraps a token read from configuration.
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The token, for the one caller that puts it on the wire.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ProjectToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProjectToken(<redacted>)")
    }
}

/// Why a process is not reporting. Logged once at boot, so an operator who
/// *expected* analytics can tell "switched off" from "misconfigured".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Silence {
    /// The operator set `OPENCOMPANY_ANALYTICS=off`.
    OptedOut,
    /// Not a hosted tenant, and nobody opted in. **The default.**
    NotHosted,
    /// Reporting was asked for, but no project token is configured.
    NoToken,
    /// `OPENCOMPANY_ANALYTICS` was set to something this does not recognise.
    ///
    /// A separate reason from [`Self::OptedOut`] on purpose: an operator who
    /// typed `of` gets the outcome they meant *and* a boot line saying their
    /// value was not understood, rather than silence they cannot distinguish
    /// from a working opt-out.
    Unreadable,
    /// `OPENCOMPANY_ANALYTICS_ENDPOINT` is set to something no client could
    /// POST to — no scheme, a scheme that is not `http`/`https`, no host, or
    /// bytes this process cannot read.
    ///
    /// Silence rather than reporting, because the alternative is the failure
    /// this whole module is built to prevent: boot prints "reporting to …",
    /// the tracker is installed, and every batch dies in `reqwest` behind a
    /// `debug!` nobody has enabled. An operator reading their own logs would
    /// have no reason to look again. Naming it as a *reason* is the only thing
    /// that turns a silent misconfiguration into one line they can act on.
    ///
    /// The reason is a constant and never quotes the value: an authenticated
    /// proxy's URL is exactly where a credential lives — see
    /// `crate::analytics::boot`.
    UnusableEndpoint,
}

impl Silence {
    /// The stable reason slug, for the boot log line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OptedOut => "operator opted out",
            Self::NotHosted => "not a hosted tenant and no explicit opt-in",
            Self::NoToken => "no project token is configured",
            Self::Unreadable => "the OPENCOMPANY_ANALYTICS value is not recognised",
            Self::UnusableEndpoint => {
                "the OPENCOMPANY_ANALYTICS_ENDPOINT value is not a usable http(s) URL"
            }
        }
    }
}

/// What this process will do.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Send nothing. No client is constructed, so nothing *can* be sent.
    Silent(Silence),
    /// Report to `endpoint` under `token`.
    Report {
        /// The collector URL.
        endpoint: String,
        /// The project token.
        token: ProjectToken,
    },
}

impl Decision {
    /// Whether this decision reports.
    pub fn reports(&self) -> bool {
        matches!(self, Self::Report { .. })
    }
}

/// Resolves the decision.
///
/// The order matters and is the whole policy:
///
/// 1. `OPENCOMPANY_ANALYTICS=off` wins over everything. An operator switching
///    it off must not be overruled by a deployment kind, a token, or a future
///    default.
/// 2. A value that is set but unrecognised resolves to **silence**, whatever
///    the deployment. The deployment default is reserved for a switch that is
///    *absent*. Falling an unreadable value through to the default meant a
///    hosted tenant whose operator typed `OPENCOMPANY_ANALYTICS=of` kept
///    reporting — a typo in the opt-out direction silently ignored, which is
///    the one direction that must never be silently ignored.
/// 3. Otherwise reporting is on **only** for [`Deployment::HostedTenant`], or
///    when an operator explicitly sets `OPENCOMPANY_ANALYTICS=on`. Decision 1
///    of #1739: silence is the default and reporting is the exception, so a
///    self-hosted or desktop install that has said nothing sends nothing.
/// 4. A token is required. Without one there is nowhere to report to, and
///    guessing is not an option — see [`TOKEN_ENV`].
/// 5. And the endpoint has to be one a client could post to. A decision that
///    says [`Decision::Report`] is a promise the boot line then repeats out
///    loud, so an endpoint that cannot be sent to is silence with a reason,
///    not reporting — see [`is_usable_endpoint`].
pub fn resolve(deployment: Deployment, env: &dyn EnvSource) -> Decision {
    // Read through `get_os`, not `get`. [`EnvSource::get`] maps a non-Unicode
    // value to `None`, which here would read as "the operator said nothing" and
    // leave a hosted tenant reporting — the same failure as the unreadable
    // spelling below, arriving by a different route. The trait's own docs point
    // a reader that must tell *malformed* from *unset* at `get_os` for exactly
    // this reason.
    //
    // Blank is still absent, for the same reason a blank token is: a variable
    // set to whitespace is a variable nobody meant to set. See [`non_blank`].
    let switch = match env.get_os(ENABLE_ENV) {
        Some(raw) => match raw.into_string() {
            Ok(value) => {
                let value = value.trim().to_ascii_lowercase();
                if value.is_empty() { None } else { Some(value) }
            }
            Err(_) => return Decision::Silent(Silence::Unreadable),
        },
        None => None,
    };

    match switch.as_deref() {
        Some("off" | "false" | "0" | "no") => return Decision::Silent(Silence::OptedOut),
        Some("on" | "true" | "1" | "yes") => {}
        // Set, but not a spelling of yes or no. Both directions of that typo
        // are now silence: it was never an opt-in, and — since it reached a
        // hosted tenant's deployment default and kept reporting — it must not
        // be a failed opt-*out* either. Silence is the safe answer to "I cannot
        // tell what you asked for", and the boot line says which value it could
        // not read.
        Some(_) => return Decision::Silent(Silence::Unreadable),
        None => {
            if deployment != Deployment::HostedTenant {
                return Decision::Silent(Silence::NotHosted);
            }
        }
    }

    let Some(token) = non_blank(env, TOKEN_ENV) else {
        return Decision::Silent(Silence::NoToken);
    };

    // A non-Unicode token already fails closed on its own: `get` maps it to
    // `None` and the check above reports `NoToken`. The endpoint is the one
    // that needed saying out loud, twice over — see below.
    let endpoint = match env.get_os(ENDPOINT_ENV) {
        None => DEFAULT_ENDPOINT.to_string(),
        // Bytes this process cannot read are not a URL it can post to, and
        // must not fall back to `DEFAULT_ENDPOINT`: an operator who pointed
        // this at their own proxy would then be reporting to Mixpanel
        // instead — telemetry sent somewhere they never configured, which is
        // worse than sending none. `get` cannot express that difference,
        // which is why this reads through `get_os`.
        Some(raw) => match raw.into_string() {
            Err(_) => return Decision::Silent(Silence::UnusableEndpoint),
            Ok(value) => match value.trim() {
                // Blank is absent, as it is for the token and the switch.
                "" => DEFAULT_ENDPOINT.to_string(),
                configured if is_usable_endpoint(configured) => configured.to_string(),
                _ => return Decision::Silent(Silence::UnusableEndpoint),
            },
        },
    };

    Decision::Report {
        endpoint,
        token: ProjectToken::new(token),
    }
}

/// Whether `raw` is something a client could actually POST a batch to: an
/// absolute `http`/`https` URL with a host.
///
/// This is the check that stops [`resolve`] promising what the transport cannot
/// deliver. `OPENCOMPANY_ANALYTICS_ENDPOINT=collector.internal/track` — a proxy
/// hostname written without a scheme, which is how anyone would first write it
/// — resolved to [`Decision::Report`]: boot said "reporting to
/// collector.internal/track", the tracker was installed, and every send failed
/// with `RelativeUrlWithoutBase` behind a `debug!` line. Nothing an operator
/// would ever see said the endpoint was the problem.
///
/// **Parsed with `url`, the same crate `reqwest` parses with, rather than
/// approximated.** The first version of this check hand-rolled the grammar to
/// avoid what it wrongly believed would be a new dependency — `url` has been an
/// unconditional one since issue #673, added there with the rule this check
/// should have followed: it must be *the same* parser `reqwest` uses, because
/// "a grant key computed by a second, hand-rolled reader is a bypass waiting to
/// be found". The hand-rolled version accepted five shapes `reqwest` rejects
/// outright:
/// `http://[::1/track` (unclosed bracket), `http://host:99999/track` and
/// `:65536` (port out of range), `http://host:abc/track`,
/// `http://host:8080:9090/track`, and `http://999.999.999.999/track`. Each one
/// resolved to `Report` and then dropped every batch — the exact failure the
/// check exists to prevent, reintroduced by the check itself. The IPv4-shaped-
/// host rule (`127.0.0.1.5` is rejected, `exa_mple.com` is not) is the tell
/// that the tail here is unbounded: an approximation of a grammar this fiddly
/// is a standing source of the same bug. One parser, and it is the transport's
/// own.
///
/// Two things are still checked beyond parsing, because `url` is happy with
/// both and `reqwest` is not:
///
/// * **the scheme.** `url` parses `ftp://collector.internal/track` and
///   `reqwest` will even *build* a request from it; the send then fails with
///   "URL scheme is not allowed". Measured, not assumed.
/// * **a non-empty host**, defensively. No input has been found where `url`
///   returns a parsed `http`/`https` URL with an empty host — `https://` is a
///   parse error, and `http:///track` is *not* the counter-example it looks
///   like, because `url` normalizes it to `http://track/`, taking the first
///   path segment as the host. The guard stays because "there is somewhere to
///   connect to" is the property actually being asserted, and it should not
///   rest on a normalization rule holding forever.
///
/// This asks a different question from the endpoint redaction in
/// `crate::analytics::boot` — that one is about what may be *printed* — so the
/// two are not two halves of one rule.
fn is_usable_endpoint(raw: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some_and(|host| !host.is_empty())
}

/// The tenant-identity key, if this deployment configured one.
///
/// Read through `get`, not `get_os`, and that is deliberate rather than an
/// oversight of the rule the switch and the endpoint follow: there is no
/// unsafe direction to fail into here. A key that cannot be read is treated as
/// absent, and absent means the host falls back to its random instance id —
/// which is *more* private than any keyed digest, not less. The distinction
/// `get_os` buys elsewhere ("malformed must not read as unset") only matters
/// when unset is the dangerous answer, and here it is the safe one.
pub fn tenant_id_key(env: &dyn EnvSource) -> Option<TenantIdKey> {
    non_blank(env, ID_KEY_ENV).and_then(TenantIdKey::new)
}

/// A configured value, trimmed, or `None` when there is nothing left of it.
///
/// [`EnvSource::get`] already drops an *empty* value, but not a whitespace-only
/// one, and the difference is not academic: a token mounted from a file arrives
/// with a trailing newline more often than not. Untrimmed, a hosted tenant whose
/// token is `"\n"` resolves to [`Decision::Report`], the boot line says
/// "reporting to …", and every batch is refused by the collector — the failure
/// mode #1739 added that line to prevent.
///
/// The endpoint is trimmed by [`resolve`] itself rather than here, because it
/// has to be read through [`EnvSource::get_os`] to tell an unreadable value from
/// an absent one.
///
/// The same trim-and-filter the rest of the tree applies to environment values
/// (`src/bin/opencompany.rs`).
fn non_blank(env: &dyn EnvSource, key: &str) -> Option<String> {
    env.get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::app::config::MapEnv;

    fn token_env(pairs: &[(&str, &str)]) -> MapEnv {
        let mut all = vec![(TOKEN_ENV, "not-a-real-token")];
        all.extend_from_slice(pairs);
        MapEnv::new(all)
    }

    /// **The decision the GPL posture rests on.** A self-hosted instance that
    /// has been handed a token — which is the easiest way to get this wrong,
    /// because a token looks like consent — still sends nothing.
    #[test]
    fn a_self_hosted_instance_is_silent_even_with_a_token() {
        assert_eq!(
            resolve(Deployment::SelfHosted, &token_env(&[])),
            Decision::Silent(Silence::NotHosted)
        );
    }

    #[test]
    fn a_desktop_instance_is_silent_even_with_a_token() {
        assert_eq!(
            resolve(Deployment::Desktop, &token_env(&[])),
            Decision::Silent(Silence::NotHosted)
        );
    }

    #[test]
    fn a_hosted_tenant_with_a_token_reports() {
        let decision = resolve(Deployment::HostedTenant, &token_env(&[]));
        assert!(decision.reports(), "{decision:?}");
        match decision {
            Decision::Report { endpoint, token } => {
                assert_eq!(endpoint, DEFAULT_ENDPOINT);
                assert_eq!(token.expose(), "not-a-real-token");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A hosted tenant with no token is misconfigured, not reporting to
    /// nowhere — and the reason says which.
    #[test]
    fn a_hosted_tenant_without_a_token_is_silent() {
        assert_eq!(
            resolve(Deployment::HostedTenant, &MapEnv::default()),
            Decision::Silent(Silence::NoToken)
        );
    }

    /// `off` outranks the deployment kind. The platform can switch a tenant off
    /// without rebuilding it.
    #[test]
    fn off_outranks_a_hosted_deployment() {
        assert_eq!(
            resolve(Deployment::HostedTenant, &token_env(&[(ENABLE_ENV, "off")])),
            Decision::Silent(Silence::OptedOut)
        );
    }

    /// The self-hoster's opt-in, which is the only way a non-hosted install ever
    /// reports.
    #[test]
    fn a_self_hoster_can_opt_in() {
        assert!(resolve(Deployment::SelfHosted, &token_env(&[(ENABLE_ENV, "on")])).reports());
    }

    /// A typo must not opt anybody in.
    #[test]
    fn a_misspelled_switch_does_not_opt_in() {
        assert_eq!(
            resolve(Deployment::SelfHosted, &token_env(&[(ENABLE_ENV, "onn")])),
            Decision::Silent(Silence::Unreadable)
        );
    }

    /// **And a typo must not fail to opt anybody out.** This is the direction
    /// that used to leak: an unreadable value fell through to the deployment
    /// default, so a hosted tenant whose operator meant `off` and typed `of`
    /// carried on reporting, with a boot line that said "reporting to …" and
    /// gave them no reason to look again.
    #[test]
    fn a_misspelled_opt_out_does_not_keep_a_hosted_tenant_reporting() {
        for typo in ["of", "offf", "disabled", "0.0", "nope"] {
            let decision = resolve(Deployment::HostedTenant, &token_env(&[(ENABLE_ENV, typo)]));
            assert_eq!(
                decision,
                Decision::Silent(Silence::Unreadable),
                "{typo:?} must not leave a hosted tenant reporting"
            );
            assert!(!decision.reports(), "{typo:?}");
        }
    }

    /// **A switch that is set but is not text fails closed too.**
    ///
    /// `EnvSource::get` maps a non-Unicode value to `None`, so reading through
    /// it would have treated `OPENCOMPANY_ANALYTICS=<invalid bytes>` as an
    /// absent switch and left a hosted tenant reporting — the same leak as the
    /// unreadable spelling, by a different route.
    #[cfg(unix)]
    #[test]
    fn a_non_unicode_switch_is_unreadable_rather_than_absent() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        struct NonUnicodeSwitch;
        impl EnvSource for NonUnicodeSwitch {
            fn get_os(&self, key: &str) -> Option<OsString> {
                match key {
                    ENABLE_ENV => Some(OsString::from_vec(vec![0xff, 0xfe, 0x6f, 0x6e])),
                    TOKEN_ENV => Some(OsString::from("not-a-real-token")),
                    _ => None,
                }
            }
        }

        // The premise: this really is a value `get` cannot see at all.
        assert_eq!(NonUnicodeSwitch.get(ENABLE_ENV), None);
        assert!(NonUnicodeSwitch.get_os(ENABLE_ENV).is_some());

        assert_eq!(
            resolve(Deployment::HostedTenant, &NonUnicodeSwitch),
            Decision::Silent(Silence::Unreadable),
            "a switch set to bytes this process cannot read must not read as unset"
        );
    }

    /// The near-miss control: `off` really is matched case-insensitively and
    /// after trimming, so the test above is finding typos rather than finding
    /// every value that is not lowercase and bare.
    #[test]
    fn an_off_switch_is_trimmed_and_case_folded() {
        assert_eq!(
            resolve(
                Deployment::HostedTenant,
                &token_env(&[(ENABLE_ENV, "  ofF\n")])
            ),
            Decision::Silent(Silence::OptedOut)
        );
    }

    /// The control for the two above: an **absent** switch still falls to the
    /// deployment default, in both directions. Without this, "everything is
    /// silent now" would pass the tests above just as well.
    #[test]
    fn an_absent_switch_still_falls_to_the_deployment_default() {
        assert!(resolve(Deployment::HostedTenant, &token_env(&[])).reports());
        assert_eq!(
            resolve(Deployment::SelfHosted, &token_env(&[])),
            Decision::Silent(Silence::NotHosted)
        );
    }

    /// A whitespace-only switch is an absent switch, not an unreadable one —
    /// consistent with the token and endpoint, and it must not flip a hosted
    /// tenant into silence just because a launcher exported an empty variable.
    #[test]
    fn a_blank_switch_is_treated_as_absent() {
        assert!(
            resolve(Deployment::HostedTenant, &token_env(&[(ENABLE_ENV, "   ")])).reports(),
            "a blank switch must not read as unreadable"
        );
    }

    /// A token that is only whitespace is not a token. This is not a theoretical
    /// value: a secret mounted from a file arrives with a trailing newline, and
    /// a hosted tenant handed a blank one must read as **misconfigured** rather
    /// than as reporting — otherwise boot prints "reporting to …" and every
    /// batch is silently refused by the collector.
    ///
    /// `EnvSource::get` already drops an *empty* value, so the whitespace-only
    /// case is the one that needs this and the one asserted here.
    #[test]
    fn a_blank_token_is_no_token() {
        for blank in ["   ", "\n", "\t\n "] {
            assert_eq!(
                resolve(Deployment::HostedTenant, &MapEnv::new([(TOKEN_ENV, blank)])),
                Decision::Silent(Silence::NoToken),
                "a token of {blank:?} must not read as configured"
            );
        }
    }

    /// And a token that merely *arrived* with surrounding whitespace is used,
    /// trimmed, rather than put on the wire with a newline in it.
    #[test]
    fn a_token_is_trimmed() {
        match resolve(
            Deployment::HostedTenant,
            &MapEnv::new([(TOKEN_ENV, "  not-a-real-token\n")]),
        ) {
            Decision::Report { token, .. } => assert_eq!(token.expose(), "not-a-real-token"),
            other => panic!("{other:?}"),
        }
    }

    /// A blank endpoint falls back to the default rather than replacing it with
    /// a URL that cannot parse — the shape of this bug that says nothing at all
    /// at boot, because the line still reads "reporting to".
    #[test]
    fn a_blank_endpoint_falls_back_to_the_default() {
        match resolve(
            Deployment::HostedTenant,
            &token_env(&[(ENDPOINT_ENV, "  \n")]),
        ) {
            Decision::Report { endpoint, .. } => assert_eq!(endpoint, DEFAULT_ENDPOINT),
            other => panic!("{other:?}"),
        }
    }

    /// The positive control for the two above, and deliberately **insensitive**
    /// to the trim: no surrounding whitespace, so this test passes both with the
    /// filter and without it. Without such a control, "every test in the group
    /// fails when I revert the fix" would be evidence that the group asserts the
    /// implementation rather than the behaviour.
    #[test]
    fn a_configured_endpoint_still_overrides() {
        match resolve(
            Deployment::HostedTenant,
            &token_env(&[(ENDPOINT_ENV, "http://127.0.0.1:9/track")]),
        ) {
            Decision::Report { endpoint, .. } => assert_eq!(endpoint, "http://127.0.0.1:9/track"),
            other => panic!("{other:?}"),
        }
    }

    /// **A malformed endpoint is silence with a reason, not reporting.**
    ///
    /// `collector.internal/track` — a proxy hostname written without a scheme,
    /// which is how anyone would first write one — used to resolve to
    /// `Decision::Report`. Boot printed "reporting to collector.internal/track",
    /// the tracker was installed, and every batch died inside `reqwest` behind a
    /// `debug!` line no operator has enabled. The product said something
    /// true-sounding and then did nothing, which is the one failure this module
    /// exists to make impossible.
    #[test]
    fn a_malformed_endpoint_is_silence_rather_than_a_broken_report() {
        for unusable in [
            "collector.internal/track",
            "collector.internal",
            "/track",
            "://collector.internal/track",
            "ftp://collector.internal/track",
            "file:///tmp/track",
            "https://",
            "http://someone:hunter2@/track",
            "http://collector internal/track",
        ] {
            let decision = resolve(
                Deployment::HostedTenant,
                &token_env(&[(ENDPOINT_ENV, unusable)]),
            );
            assert_eq!(
                decision,
                Decision::Silent(Silence::UnusableEndpoint),
                "{unusable:?} must not resolve to a report that cannot be sent"
            );
            assert!(!decision.reports(), "{unusable:?}");
        }
    }

    /// The reason names the variable and **never the value**: an authenticated
    /// proxy carries its key in the very URL that was rejected, so quoting the
    /// bad value would put a credential in the boot line of every
    /// misconfigured tenant. Asserted case-insensitively, because a guard that
    /// matched exact case would read a lowercased leak as clean.
    #[test]
    fn the_unusable_endpoint_reason_never_quotes_the_endpoint() {
        const SECRET: &str = "NotARealCollectorKey";
        let reason = Silence::UnusableEndpoint.as_str();
        assert!(
            reason.contains("OPENCOMPANY_ANALYTICS_ENDPOINT"),
            "the reason must name the variable to act on: {reason}"
        );

        // Rejected for having no scheme, and carrying a credential while it is
        // rejected — which is exactly the case that would leak.
        let raw = format!("collector.internal/track?key={SECRET}");
        assert_eq!(
            resolve(
                Deployment::HostedTenant,
                &token_env(&[(ENDPOINT_ENV, raw.as_str())])
            ),
            Decision::Silent(Silence::UnusableEndpoint)
        );
        let printed = format!("{:?} {}", Silence::UnusableEndpoint, reason);
        assert!(
            !printed
                .to_ascii_lowercase()
                .contains(&SECRET.to_ascii_lowercase()),
            "the reason leaked the endpoint credential: {printed}"
        );
        // The self-check: the needle really is findable in the unredacted
        // value, in whatever case it comes back, or the guard above is vacuous.
        assert!(
            raw.to_ascii_lowercase()
                .contains(&SECRET.to_ascii_lowercase())
                && raw
                    .to_ascii_uppercase()
                    .to_ascii_lowercase()
                    .contains(&SECRET.to_ascii_lowercase()),
            "the needle must be findable before redaction: {raw}"
        );
    }

    /// **A non-Unicode endpoint is unusable, not absent.**
    ///
    /// `EnvSource::get` maps it to `None`, which fell back to
    /// `DEFAULT_ENDPOINT` — so a tenant that pointed analytics at its own proxy
    /// and mistyped the bytes reported to **Mixpanel** instead. Telemetry sent
    /// somewhere the operator never configured is worse than telemetry not sent
    /// at all, and it is the one outcome that no amount of reading the boot
    /// line would have revealed: the line named a destination that was real.
    #[cfg(unix)]
    #[test]
    fn a_non_unicode_endpoint_does_not_silently_fall_back_to_mixpanel() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        struct NonUnicodeEndpoint;
        impl EnvSource for NonUnicodeEndpoint {
            fn get_os(&self, key: &str) -> Option<OsString> {
                match key {
                    ENDPOINT_ENV => Some(OsString::from_vec(
                        [b"https://collector.invalid/".as_slice(), &[0xff, 0xfe]].concat(),
                    )),
                    TOKEN_ENV => Some(OsString::from("not-a-real-token")),
                    _ => None,
                }
            }
        }

        // The premise: a value `get` cannot see at all.
        assert_eq!(NonUnicodeEndpoint.get(ENDPOINT_ENV), None);
        assert!(NonUnicodeEndpoint.get_os(ENDPOINT_ENV).is_some());

        let decision = resolve(Deployment::HostedTenant, &NonUnicodeEndpoint);
        assert_eq!(decision, Decision::Silent(Silence::UnusableEndpoint));
        match &decision {
            Decision::Report { endpoint, .. } => {
                panic!("reported to {endpoint} — an endpoint the operator never configured")
            }
            Decision::Silent(_) => {}
        }
    }

    /// **The endpoint check agrees with what `reqwest` can actually send to.**
    ///
    /// Every row was measured against reqwest 0.12.28 — `Url::parse`,
    /// `Client::post(..).build()`, and for the scheme, what the send does — not
    /// reasoned about. The rows marked below are the ones a hand-rolled grammar
    /// check accepted and `reqwest` rejects; they resolved to `Decision::Report`
    /// and then dropped every batch, which is the very failure
    /// `is_usable_endpoint` exists to prevent.
    #[test]
    fn the_endpoint_check_matches_what_the_transport_accepts() {
        // (endpoint, usable) — `false` means `reqwest` cannot send to it.
        let measured: &[(&str, bool)] = &[
            // Rejected by `Url::parse`. Each of these was accepted by the
            // hand-rolled check this replaced.
            ("http://[::1/track", false),  // unclosed IPv6 bracket
            ("http://]::1[/track", false), // brackets inside out
            ("http://collector.internal:99999/track", false), // port out of range
            ("http://collector.internal:65536/track", false), // one past the top
            ("http://collector.internal:abc/track", false), // port not a number
            ("http://host:8080:9090/track", false), // two ports
            ("http://127.0.0.1.5/track", false), // IPv4-shaped, invalid
            ("http://999.999.999.999/track", false), // IPv4-shaped, invalid
            // Rejected by `Url::parse` and by the hand-rolled check alike.
            ("collector.internal/track", false),
            ("collector.internal", false),
            ("/track", false),
            ("://collector.internal/track", false),
            ("https://", false),
            ("http://someone:hunter2@/track", false),
            ("http://collector internal/track", false),
            // Parsed happily by `url` — and even built by `reqwest` — but not
            // sendable, so checked on top of the parse.
            ("ftp://collector.internal/track", false), // scheme refused at send
            ("file:///tmp/track", false),
            // NOT here: `http:///track`. It looks like an empty host and is
            // not one — `url` normalizes it to `http://track/`, taking the
            // first path segment as the host, and `reqwest` sends to it. A
            // collector named `track` that does not resolve is an unreachable
            // collector like any other, which #1739 makes a no-op on purpose.
            // Accepted, and the ones a deployment actually uses.
            (DEFAULT_ENDPOINT, true),
            ("http://127.0.0.1:9/track", true),
            ("http://127.0.0.1:9", true),
            ("http://collector.internal:65535/track", true), // the top of the range
            ("http://collector.internal:/track", true),      // empty port is legal
            ("https://collector.internal/track", true),
            ("HTTPS://collector.internal/track", true),
            (
                "https://collector.internal/track?key=NotARealCollectorKey",
                true,
            ),
            (
                "https://someone:NotARealCollectorKey@collector.internal/track",
                true,
            ),
            ("https://[::1]:8443/track", true),
            ("http://[::1]/track", true),
            ("https://collector.internal:8443/track#frag", true),
            // Odd but legal, and deliberately still accepted: rejecting these
            // would silence a working deployment, which is the direction that
            // costs more than it saves.
            ("http://exa_mple.com/track", true),
            ("http://-example.com/track", true),
            ("http://\u{4f8b}\u{3048}.jp/track", true),
        ];

        for (endpoint, usable) in measured {
            let decision = resolve(
                Deployment::HostedTenant,
                &token_env(&[(ENDPOINT_ENV, endpoint)]),
            );
            if *usable {
                match decision {
                    Decision::Report { endpoint: got, .. } => assert_eq!(&got, endpoint),
                    other => panic!("{endpoint:?} must still report: {other:?}"),
                }
            } else {
                assert_eq!(
                    decision,
                    Decision::Silent(Silence::UnusableEndpoint),
                    "{endpoint:?} cannot be sent to, so it must not resolve to a report"
                );
            }
        }
    }

    /// The controls that keep the group above from passing by rejecting
    /// everything: the endpoints a deployment actually uses still resolve, and
    /// still resolve to themselves.
    #[test]
    fn a_usable_endpoint_still_reports_to_exactly_itself() {
        for usable in [
            DEFAULT_ENDPOINT,
            "http://127.0.0.1:9/track",
            "http://127.0.0.1:9",
            "https://collector.internal/track",
            "HTTPS://collector.internal/track",
            "https://collector.internal/track?key=NotARealCollectorKey",
            "https://someone:NotARealCollectorKey@collector.internal/track",
            "https://[::1]:8443/track",
            "https://collector.internal:8443/track#frag",
        ] {
            match resolve(
                Deployment::HostedTenant,
                &token_env(&[(ENDPOINT_ENV, usable)]),
            ) {
                Decision::Report { endpoint, .. } => assert_eq!(endpoint, usable),
                other => panic!("{usable:?} must still report: {other:?}"),
            }
        }
    }

    /// The token is a credential: it must not be printable by accident, because
    /// the accident is a `{:?}` in a log line nobody reviewed.
    #[test]
    fn a_token_is_not_printable() {
        let token = ProjectToken::new("not-a-real-token");
        let printed = format!("{token:?}");
        assert!(
            !printed.contains("not-a-real-token"),
            "the Debug impl leaked the token: {printed}"
        );

        let decision = Decision::Report {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            token,
        };
        let printed = format!("{decision:?}");
        assert!(
            !printed.contains("not-a-real-token"),
            "the Debug impl leaked the token through the decision: {printed}"
        );
    }
}
