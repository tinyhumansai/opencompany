//! [`Deployment`]: which *kind* of install this process is.
//!
//! Three deployments run this same binary and they are not interchangeable:
//! a tenant container the hosting platform provisioned, an operator's own
//! self-hosted server, and the desktop app's embedded host. Behaviour that is
//! correct for one is wrong for another — most sharply for analytics
//! (`docs/spec/runtime/analytics.md`), where a hosted tenant reporting to the
//! platform that runs it is ordinary operations and a self-hosted GPL install
//! doing the same thing is a betrayal.
//!
//! ## Why this is declared, not sniffed
//!
//! It would be easy to infer the kind from something already present —
//! `harness_in_build` differs between the desktop and the server today, the
//! data dir is `/data` in a container, the bind address is `0.0.0.0`. Every one
//! of those is a coincidence that inverts the day someone changes an unrelated
//! setting: the desktop enables the harness, an operator mounts `/data`, a
//! self-hoster binds all interfaces behind their own proxy. A discriminator
//! whose meaning depends on an unrelated decision is worse than none, because
//! the failure is silent and points at the wrong file.
//!
//! So it is **declared** by whoever launches the process, through
//! `OPENCOMPANY_DEPLOYMENT`, and the default is the one that is safe to be
//! wrong about: [`Deployment::SelfHosted`], which sends nothing.
//!
//! One inference is allowed, and only one: `OPENCOMPANY_TENANT_ID` is injected
//! by the control plane and by nothing else (`CLAUDE.md`, shared-single-DB
//! mode), so its presence names a hosted tenant. It is a fallback for tenants
//! whose manager predates `OPENCOMPANY_DEPLOYMENT`, not the primary signal —
//! db-per-tenant tenants do not set it, which is exactly why it cannot be the
//! only answer.

use crate::app::config::EnvSource;

/// The environment variable that declares the deployment kind.
pub const DEPLOYMENT_ENV: &str = "OPENCOMPANY_DEPLOYMENT";

/// The variable the control plane injects in shared-single-DB mode. Read here
/// only as a fallback signal for "this is a hosted tenant".
const TENANT_ENV: &str = "OPENCOMPANY_TENANT_ID";

/// Which kind of install this process is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Deployment {
    /// The desktop app's embedded host. One human, on their own machine.
    Desktop,
    /// An operator running this GPL-3.0 crate on their own infrastructure.
    /// **The default**, because it is the kind that must never phone home by
    /// accident.
    #[default]
    SelfHosted,
    /// A per-tenant container the OpenCompany hosting platform provisioned and
    /// operates.
    HostedTenant,
}

impl Deployment {
    /// The stable slug for this kind. `&'static str` on purpose: it is a
    /// telemetry property, and every analytics property value in this crate is
    /// a compile-time constant so that no caller-supplied text can reach one.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::SelfHosted => "self-hosted",
            Self::HostedTenant => "hosted-tenant",
        }
    }

    /// Parses a declared slug. Unknown text is **not** an error and **not** a
    /// guess: it resolves to [`Self::SelfHosted`], the silent default. A typo in
    /// a launcher must not upgrade an install into one that reports.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "desktop" => Self::Desktop,
            "hosted-tenant" | "hosted_tenant" | "hosted" => Self::HostedTenant,
            _ => Self::SelfHosted,
        }
    }

    /// Resolves the deployment kind from the environment.
    ///
    /// `OPENCOMPANY_DEPLOYMENT` wins outright — **including when this process
    /// cannot read it**. Failing that, a tenant namespace names a hosted tenant
    /// (see the module docs for why that inference is the only one taken).
    /// Everything else is self-hosted.
    pub fn from_env(env: &dyn EnvSource) -> Self {
        // Read through `get_os`, not `get`. [`EnvSource::get`] maps a
        // non-Unicode value to `None`, which here read as "nobody declared
        // anything" and fell through to the tenant inference below — so an
        // unreadable declaration alongside `OPENCOMPANY_TENANT_ID`
        // (shared-single-DB mode) returned `HostedTenant` and switched
        // reporting **on**. A fail-open on the discriminator the whole
        // analytics posture rests on, arriving by a route [`Self::parse`] never
        // sees.
        //
        // A declaration this process cannot read is still a declaration: it
        // wins, and it resolves to the silent default. When we cannot tell what
        // kind of install this is, the safe answer is the one that sends
        // nothing. `crate::analytics::config::resolve` reads its own switch
        // through `get_os` for exactly this reason, and the trait's own docs
        // point every reader that must tell *malformed* from *unset* at it.
        //
        // Empty stays absent, exactly as `EnvSource::get` treats it and exactly
        // as the analytics switch treats a blank one: a launcher that exported
        // the variable without a value has said nothing, and must not cost a
        // hosted tenant the inference it would otherwise have had.
        //
        // **Whitespace-only stays absent too**, and for the same reason it does
        // there: a launcher that mounts this from a file hands it over with a
        // trailing newline more often than not, and `"\n"` is not empty. Left
        // untrimmed, a hosted tenant whose file ended in a newline fell through
        // `parse` to `SelfHosted` and silently lost the tenant inference — while
        // `crate::analytics::config::resolve` trims before deciding, so the two
        // readers disagreed about the same input shape. A value that is not
        // valid UTF-8 is still *malformed rather than absent* and still answers
        // `SelfHosted`, which is the distinction this whole block exists for.
        if let Some(raw) = env
            .get_os(DEPLOYMENT_ENV)
            .filter(|raw| !raw.is_empty())
            .filter(|raw| raw.to_str().is_none_or(|text| !text.trim().is_empty()))
        {
            return raw
                .into_string()
                .map_or(Self::SelfHosted, |declared| Self::parse(&declared));
        }
        if env.get(TENANT_ENV).is_some() {
            return Self::HostedTenant;
        }
        Self::SelfHosted
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::app::config::MapEnv;

    /// The load-bearing default. Everything about the analytics posture rests on
    /// an unconfigured process being self-hosted, so this is pinned rather than
    /// left to `#[derive(Default)]` being read correctly by the next person.
    #[test]
    fn an_undeclared_deployment_is_self_hosted() {
        assert_eq!(
            Deployment::from_env(&MapEnv::default()),
            Deployment::SelfHosted
        );
        assert_eq!(Deployment::default(), Deployment::SelfHosted);
    }

    #[test]
    fn a_declaration_wins_over_the_tenant_inference() {
        let env = MapEnv::new([
            ("OPENCOMPANY_DEPLOYMENT", "desktop"),
            ("OPENCOMPANY_TENANT_ID", "acme"),
        ]);
        assert_eq!(Deployment::from_env(&env), Deployment::Desktop);
    }

    #[test]
    fn a_tenant_namespace_names_a_hosted_tenant() {
        let env = MapEnv::new([("OPENCOMPANY_TENANT_ID", "acme")]);
        assert_eq!(Deployment::from_env(&env), Deployment::HostedTenant);
    }

    /// A **whitespace-only** declaration is absent, not a declaration.
    ///
    /// `"\n"` is not empty, so the length filter alone let it through to
    /// `parse`, which trims it to `""`, matches no arm and answers
    /// `SelfHosted` — costing a hosted tenant the inference it would otherwise
    /// have had. A launcher that mounts this variable from a file hands it over
    /// with a trailing newline more often than not, so this is the ordinary
    /// shape rather than a contrived one, and `analytics::config::resolve`
    /// already trims before deciding — the two readers disagreed about the same
    /// input.
    #[test]
    fn a_whitespace_only_declaration_is_absent() {
        for blank in ["\n", " ", "\t\n ", "\r\n"] {
            assert_eq!(
                Deployment::from_env(&MapEnv::new([(DEPLOYMENT_ENV, blank)])),
                Deployment::SelfHosted,
                "with nothing else to go on: {blank:?}"
            );
            // The point of the fix: the tenant inference survives it.
            assert_eq!(
                Deployment::from_env(&MapEnv::new([
                    (DEPLOYMENT_ENV, blank),
                    ("OPENCOMPANY_TENANT_ID", "acme"),
                ])),
                Deployment::HostedTenant,
                "a blank declaration must not outrank the tenant namespace: {blank:?}"
            );
        }
        // A real declaration still wins, padding and all.
        assert_eq!(
            Deployment::from_env(&MapEnv::new([
                (DEPLOYMENT_ENV, " desktop\n"),
                ("OPENCOMPANY_TENANT_ID", "acme"),
            ])),
            Deployment::Desktop,
        );
    }

    /// A typo must fall to silence, never to reporting. The dangerous direction
    /// is the only one worth a test.
    ///
    /// `OPENCOMPANY_TENANT_ID` is set on purpose: the interesting question is
    /// not whether `parse` maps an unknown slug to `SelfHosted` — it plainly
    /// does — but whether an unrecognised declaration **wins over** the tenant
    /// inference rather than falling through to it. Without the tenant variable
    /// this test passes either way and proves nothing about the fall-through,
    /// which is the shape the non-Unicode leak below actually had.
    #[test]
    fn an_unrecognised_declaration_falls_back_to_silence() {
        for typo in [
            "hosted-tenat",
            "hosted-tennant",
            "hosted tenant",
            "Hosted-Tenent",
        ] {
            let env = MapEnv::new([
                ("OPENCOMPANY_DEPLOYMENT", typo),
                ("OPENCOMPANY_TENANT_ID", "acme"),
            ]);
            assert_eq!(
                Deployment::from_env(&env),
                Deployment::SelfHosted,
                "{typo:?} must not fall through to the tenant inference"
            );
        }
    }

    /// **A declaration that is set but is not text fails closed too.**
    ///
    /// `EnvSource::get` maps a non-Unicode value to `None`, so reading through
    /// it treated `OPENCOMPANY_DEPLOYMENT=<invalid bytes>` as an absent
    /// declaration, fell through to the tenant inference, and returned
    /// `HostedTenant` — turning reporting **on** for an install whose operator
    /// had explicitly declared something. The same leak as the unrecognised
    /// spelling above, by a different route, and the one direction of failure
    /// this discriminator must never take.
    #[cfg(unix)]
    #[test]
    fn a_non_unicode_declaration_falls_closed_to_self_hosted() {
        use crate::app::config::EnvSource;
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        struct NonUnicodeDeclaration;
        impl EnvSource for NonUnicodeDeclaration {
            fn get_os(&self, key: &str) -> Option<OsString> {
                match key {
                    // `0xff 0xfe` is not valid UTF-8 in any position; the tail
                    // spells `hosted-tenant`, so a lossy read would have said
                    // the operator asked for a hosted tenant.
                    DEPLOYMENT_ENV => Some(OsString::from_vec(
                        [&[0xff, 0xfe][..], b"hosted-tenant"].concat(),
                    )),
                    TENANT_ENV => Some(OsString::from("acme")),
                    _ => None,
                }
            }
        }

        // The premise: this really is a value `get` cannot see at all, and the
        // tenant inference really is standing by to answer for it.
        assert_eq!(NonUnicodeDeclaration.get(DEPLOYMENT_ENV), None);
        assert!(NonUnicodeDeclaration.get_os(DEPLOYMENT_ENV).is_some());
        assert_eq!(
            Deployment::from_env(&MapEnv::new([("OPENCOMPANY_TENANT_ID", "acme")])),
            Deployment::HostedTenant,
            "without the declaration this environment resolves to a hosted tenant, \
             so the assertion below is about the declaration and nothing else"
        );

        assert_eq!(
            Deployment::from_env(&NonUnicodeDeclaration),
            Deployment::SelfHosted,
            "a declaration set to bytes this process cannot read must not read as unset"
        );
    }

    /// The control for the two above: a **blank** declaration is an absent one,
    /// not an unreadable one — consistent with `EnvSource::get`, with the
    /// analytics switch, and with the rest of the tree. A launcher that
    /// exported `OPENCOMPANY_DEPLOYMENT=` has said nothing, and must not cost a
    /// hosted tenant its inference. Without this control, "everything is
    /// self-hosted now" would pass the two tests above just as well.
    #[test]
    fn a_blank_declaration_is_treated_as_absent() {
        let env = MapEnv::new([
            ("OPENCOMPANY_DEPLOYMENT", ""),
            ("OPENCOMPANY_TENANT_ID", "acme"),
        ]);
        assert_eq!(Deployment::from_env(&env), Deployment::HostedTenant);
    }

    /// And the other control: a **recognised** declaration still resolves to
    /// the kind it names, so the fail-closed paths above are finding malformed
    /// values rather than refusing every declaration.
    #[test]
    fn a_recognised_declaration_still_resolves() {
        for (declared, expected) in [
            ("hosted-tenant", Deployment::HostedTenant),
            ("  Hosted-Tenant\n", Deployment::HostedTenant),
            ("hosted", Deployment::HostedTenant),
            ("desktop", Deployment::Desktop),
            ("self-hosted", Deployment::SelfHosted),
        ] {
            let env = MapEnv::new([("OPENCOMPANY_DEPLOYMENT", declared)]);
            assert_eq!(Deployment::from_env(&env), expected, "{declared:?}");
        }
    }

    #[test]
    fn every_kind_has_a_stable_slug() {
        assert_eq!(Deployment::Desktop.as_str(), "desktop");
        assert_eq!(Deployment::SelfHosted.as_str(), "self-hosted");
        assert_eq!(Deployment::HostedTenant.as_str(), "hosted-tenant");
    }
}
