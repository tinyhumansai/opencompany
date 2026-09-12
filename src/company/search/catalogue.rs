//! The four search providers a company can connect, as data.
//!
//! # Where these facts come from
//!
//! The endpoint and the auth header are read off **the vendored OpenHuman tool
//! that makes the call**, not off the provider's published documentation,
//! because the vendored tool is what runs. Where the two could disagree — Exa
//! accepts `Authorization: Bearer` as well as `x-api-key`, and
//! [`crate::harness::built_in::search_byo`] sends the latter — this table
//! records what is actually sent.
//!
//! The failure shapes below come from the providers' own docs, and one of them
//! is the reason [`super::probe`] classifies per provider instead of matching
//! one regex over an error string: **Brave rejects a bad key with `422`**, and
//! its API reference documents no `401` and no `403` at all. A classifier that
//! maps "credential problem" to 401/403 would leave a dead Brave key stored and
//! would read Brave's only possible `403` — a WAF — as a rejected key.
//!
//! There is deliberately **no key-prefix field**. None of the three account
//! providers documents one, and a validation rule invented from a blog post
//! rejects a valid key for a reason the operator cannot see.
//!
//! `managed` is not in this table. It has no endpoint the company owns, no key
//! the company pastes and no row to add — it is rendered from resolution. See
//! `docs/modules/search/data-model.md`.

/// Which of the two questions a provider answers.
///
/// The split is the add dialog's, and it is the same argument the inference
/// dialog makes for its three: an account wants a key, a self-hosted instance
/// wants an address. One flat list would make the operator infer that from a
/// group heading; a select per category has a label and a line of helper text to
/// say it outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// A hosted search API behind an account. The operator supplies a key.
    Account,
    /// The operator's own instance. They supply the address; there is no key.
    SelfHosted,
}

impl Category {
    /// The wire spelling, shared with the console mirror.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Account => "account",
            Category::SelfHosted => "self-hosted",
        }
    }
}

/// How a provider expects its credential presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    /// A bare key in the named header — Brave's `X-Subscription-Token`, Exa's
    /// `x-api-key`.
    Header(&'static str),
    /// `Authorization: Bearer <key>`.
    Bearer,
}

/// One catalogue entry.
#[derive(Debug, Clone, Copy)]
pub struct SearchProviderInfo {
    /// The slug. Identity **and** address: the harness dispatches on it
    /// (`match config.provider.as_str()` in `search_byo`), so it is fixed here
    /// rather than chosen by an operator.
    pub slug: &'static str,
    /// Display name.
    pub label: &'static str,
    /// Which of the two questions this provider answers.
    pub category: Category,
    /// The API host, for the add dialog's detail line and for the probe.
    ///
    /// Not configurable for an account provider: Brave's base URL is a `const`
    /// in the vendored tool and its constructor takes no URL at all, while Exa's
    /// and Querit's constructors accept one that `search_byo` passes as `None`.
    /// A base-URL field on those rows would be a field the request ignores.
    pub endpoint: &'static str,
    /// How the credential is presented, or `None` where there is no credential.
    pub auth: Option<AuthStyle>,
    /// Where an operator gets a key, or `None` where no authoritative URL is
    /// documented.
    pub key_source: Option<&'static str>,
}

impl SearchProviderInfo {
    /// Whether this provider authenticates with a key the company pastes.
    pub fn needs_key(&self) -> bool {
        self.auth.is_some()
    }

    /// Whether this provider is addressed by an operator-supplied URL.
    pub fn needs_endpoint(&self) -> bool {
        matches!(self.category, Category::SelfHosted)
    }
}

/// Every provider that can appear as a row.
///
/// Mirrored in `frontend/src/search-providers/catalogue.ts`, with a test
/// asserting the two agree — the inference catalogue's fourth known defect is a
/// hand-duplicated table with nothing noticing when the copies drift.
pub const CATALOGUE: [SearchProviderInfo; 4] = [
    SearchProviderInfo {
        slug: "brave",
        label: "Brave Search",
        category: Category::Account,
        endpoint: "https://api.search.brave.com/res/v1",
        auth: Some(AuthStyle::Header("X-Subscription-Token")),
        key_source: Some("https://api-dashboard.search.brave.com/app/keys"),
    },
    SearchProviderInfo {
        slug: "exa",
        label: "Exa",
        category: Category::Account,
        endpoint: "https://api.exa.ai",
        auth: Some(AuthStyle::Header("x-api-key")),
        key_source: Some("https://dashboard.exa.ai/api-keys"),
    },
    SearchProviderInfo {
        slug: "querit",
        label: "Querit",
        category: Category::Account,
        endpoint: "https://api.querit.ai/v1",
        auth: Some(AuthStyle::Bearer),
        // Querit's docs say to copy the key "from the Dashboard" and never print
        // a URL for it. Linking a guess is worse than linking nothing.
        key_source: Some("https://www.querit.ai/"),
    },
    SearchProviderInfo {
        slug: "searxng",
        label: "SearXNG",
        category: Category::SelfHosted,
        // Replaced by the company's own instance URL. Present so the add
        // dialog's detail line has something to say about what SearXNG is.
        endpoint: "",
        auth: None,
        key_source: None,
    },
];

/// The catalogue entry for `slug`, if this build knows it.
pub fn entry(slug: &str) -> Option<&'static SearchProviderInfo> {
    CATALOGUE.iter().find(|info| info.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_account_provider_has_an_endpoint_a_key_source_and_an_auth_style() {
        for info in CATALOGUE.iter().filter(|i| i.category == Category::Account) {
            assert!(info.auth.is_some(), "{} has no auth style", info.slug);
            assert!(
                info.endpoint.starts_with("https://"),
                "{} endpoint is not https",
                info.slug
            );
            assert!(info.key_source.is_some(), "{} has no key source", info.slug);
            assert!(info.needs_key(), "{}", info.slug);
            assert!(!info.needs_endpoint(), "{}", info.slug);
        }
    }

    #[test]
    fn searxng_is_an_address_rather_than_an_account() {
        let info = entry("searxng").expect("searxng");
        assert!(info.auth.is_none());
        assert!(!info.needs_key());
        assert!(info.needs_endpoint());
        assert!(info.endpoint.is_empty());
    }

    #[test]
    fn managed_is_not_a_catalogue_entry() {
        // It has no endpoint the company owns and no key it pastes, so there is
        // nothing to add. The row is rendered from resolution instead.
        assert!(entry(super::super::MANAGED_PROVIDER).is_none());
    }

    /// The Rust table and its TypeScript mirror must not drift.
    ///
    /// The inference rework's fourth known defect is a provider table duplicated
    /// by hand across the language boundary with nothing noticing when the
    /// copies disagree — a provider added to one and not the other half-works.
    /// This catalogue is four rows, so there is no excuse for repeating it.
    ///
    /// Reads the mirror as text rather than parsing TypeScript: the property
    /// this holds is that the same four slugs, labels and categories appear on
    /// both sides, and a regex over a `const` array is enough to fail loudly
    /// when one is added to only one of them.
    /// The console mirror, found by walking up from this crate's manifest.
    ///
    /// **Not `CARGO_MANIFEST_DIR/frontend`.** That only resolves when the
    /// manifest directory *is* the repo root, which it is for a plain `cargo
    /// test` at the top level and is not in CI, where the crate is built from
    /// `crates/opencompany-core` — so the test passed locally and panicked on
    /// the `Rust` lane with "cannot read …".
    fn console_mirror() -> Option<std::path::PathBuf> {
        let mut dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        loop {
            let candidate = dir.join("frontend/src/search-providers/catalogue.ts");
            if candidate.is_file() {
                return Some(candidate);
            }
            dir = dir.parent()?;
        }
    }

    #[test]
    fn the_console_mirror_lists_the_same_providers() {
        let mirror = console_mirror()
            .expect("the console mirror must exist somewhere above this crate's manifest");
        let source = std::fs::read_to_string(&mirror)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", mirror.display()));

        for info in CATALOGUE {
            assert!(
                source.contains(&format!("slug: \"{}\"", info.slug)),
                "`{}` is in the Rust catalogue and not in the console mirror",
                info.slug
            );
            assert!(
                source.contains(&format!("label: \"{}\"", info.label)),
                "`{}` has a different label in the console mirror",
                info.slug
            );
            assert!(
                source.contains(&format!("category: \"{}\"", info.category.as_str())),
                "`{}`'s category is missing from the console mirror",
                info.slug
            );
        }

        let mirrored = source.matches("slug: \"").count();
        assert_eq!(
            mirrored,
            CATALOGUE.len(),
            "the console mirror lists {mirrored} providers and the Rust catalogue lists {}",
            CATALOGUE.len()
        );
    }

    #[test]
    fn every_catalogue_slug_is_a_supported_provider() {
        for info in CATALOGUE {
            assert!(
                super::super::provider_supported(info.slug),
                "{} is in the catalogue but not in SUPPORTED_PROVIDERS",
                info.slug
            );
        }
    }
}
