//! A company's **own** search connection: the BYO half of the search surface
//! (`issue #238` deferred it explicitly — "wiring it belongs with the console
//! credential surface that `composio` already uses").
//!
//! # What this inherits, and what it does not
//!
//! OpenHuman owns the search domain (`oh::search`): six engines, a canonical
//! `web_search_tool` slot, `managed` backend-proxied by default. The BYO engines
//! there — Brave, Exa, Querit, plus the standalone SearXNG tool — are ordinary
//! `Tool` implementations with public constructors that take a key and nothing
//! else. This module **calls those constructors** with the company's own stored
//! key. No provider trait, no HTTP client, no result parsing of its own: the
//! whole point is that a Brave result rendered for an OpenCompany agent is the
//! same text OpenHuman renders.
//!
//! What it does *not* do is call [`oh::search::build_search_tools`], for the
//! reason [`search`](crate::harness::search) already gives: that entry point
//! takes OpenHuman's global `Config`, and the harness assembles per-company
//! state instead of a process-wide config — two companies on one host search
//! through two different accounts, so a global is not merely awkward here, it is
//! wrong.
//!
//! # One name, whichever provider
//!
//! Every provider's canonical "search the web" tool is presented to the model as
//! **`web_search`** — the same name the managed surface uses — through
//! [`AliasedTool`]. A company that switches from managed to Brave changes what
//! the tool costs and who bills it; it does not change what the agent is told it
//! can do. The shipped research skills name `web_search` in their instructions,
//! and a belt where that name appears and disappears with a settings change is
//! how an agent comes to invent URLs instead of searching for them.
//!
//! Provider extras keep their upstream names (`exa_find_similar`,
//! `exa_get_contents`, `brave_news_search`, `brave_image_search`,
//! `brave_video_search`) — they are genuinely different affordances, and a name
//! borrowed from upstream is one an operator can look up.
//!
//! # Fail open to managed, never to nothing
//!
//! Resolution answers `None` when the company configured nothing, or configured
//! a provider whose credential is missing. The caller then wires the metered
//! managed surface, which is exactly what OpenHuman does ("a BYO engine with no
//! key falls back to the managed surface"). A half-configured settings page
//! therefore degrades to a working, capped search rather than to an agent with
//! no way to find a source.
//!
//! # Money, and why the daily cap does not follow
//!
//! The managed tool is metered and daily-capped because every call spends the
//! *platform's* money ([`search`](crate::harness::search) explains the ledger).
//! A BYO call spends the *company's* own account, billed by Brave or Exa
//! directly, under rate limits that company chose. Applying the platform's cap
//! to it would be this host throttling a bill it does not pay, so it does not:
//! the cap travels with the managed credential, and a company that wants a
//! ceiling on its own key sets one where the key is issued.

use std::sync::Arc;

use crate::company::search::{
    API_KEY_SECRET, ENDPOINT_SECRET, MANAGED_PROVIDER, PROVIDER_SECRET, configuration_complete,
    provider_is_byo,
};
use crate::ports::SecretStore;
use crate::ports::types::CompanyId;

/// Results a BYO provider is asked for when the caller does not say. Matches the
/// managed tool's default so switching providers does not change how much
/// context one search costs.
const DEFAULT_MAX_RESULTS: usize = 5;

/// Seconds a BYO provider call may take before it is abandoned. Deliberately
/// shorter than a turn: a search that has not answered in half a minute has
/// already cost the agent more than the answer is worth.
const TIMEOUT_SECS: u64 = 30;

/// Language a SearXNG instance is queried in when the company sets none.
const SEARXNG_LANGUAGE: &str = "all";

/// One company's resolved BYO search connection.
///
/// Only ever constructed for a provider that is both BYO and complete — see
/// [`TenantSearch::resolve`]. `managed`, and every half-configured provider,
/// resolve to `None` rather than to a `TenantSearch` that would wire a tool with
/// no credential behind it.
#[derive(Clone)]
pub struct TenantSearch {
    provider: String,
    api_key: Option<String>,
    endpoint: Option<String>,
}

/// Prints the provider and endpoint, never the key.
impl std::fmt::Debug for TenantSearch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TenantSearch")
            .field("provider", &self.provider)
            .field("endpoint", &self.endpoint)
            .field(
                "api_key",
                &if self.api_key.is_some() {
                    "<redacted>"
                } else {
                    "<unset>"
                },
            )
            .finish()
    }
}

impl TenantSearch {
    /// Resolves a company's BYO search connection from its secret store.
    ///
    /// `Ok(None)` means "search through the managed surface": no provider
    /// stored, `managed` stored explicitly, an unknown slug, or a BYO provider
    /// whose credential half is missing. All four are ordinary states of a
    /// settings page, not errors.
    ///
    /// A store **read failure** is an `Err`, not `Ok(None)`. Collapsing them
    /// would make an unhealthy secret store indistinguishable from "not
    /// configured", and the caller's response differs: absence should fall back
    /// to managed, while a transient read error should keep the connection the
    /// roster already had. See `HarnessPool::resolve_tenant_search`.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret store cannot be read.
    pub async fn resolve(
        secrets: &Arc<dyn SecretStore>,
        company: &CompanyId,
    ) -> crate::error::Result<Option<TenantSearch>> {
        let read = async |key: &str| -> crate::error::Result<Option<String>> {
            Ok(secrets
                .get(company, key)
                .await?
                .map(|value| value.0.trim().to_string())
                .filter(|value| !value.is_empty()))
        };

        let provider = read(PROVIDER_SECRET)
            .await?
            .unwrap_or_else(|| MANAGED_PROVIDER.to_string());
        if !provider_is_byo(&provider) {
            return Ok(None);
        }

        let api_key = read(API_KEY_SECRET).await?;
        let endpoint = read(ENDPOINT_SECRET).await?;
        if !configuration_complete(&provider, api_key.is_some(), endpoint.is_some()) {
            tracing::warn!(
                company = %company,
                provider = %provider,
                "[search] BYO provider is selected but its credential is missing; falling back to \
                 the managed surface"
            );
            return Ok(None);
        }

        Ok(Some(TenantSearch {
            provider,
            api_key,
            endpoint,
        }))
    }

    /// The provider this company searches through. Never the key.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// A connection assembled directly, for tests that need one without a
    /// secret store behind it — the roster-build and `build_agent` tests, which
    /// are about what a *resolved* connection wires rather than about how it
    /// resolved.
    #[cfg(test)]
    pub fn for_test(provider: &str, api_key: Option<&str>, endpoint: Option<&str>) -> Self {
        Self {
            provider: provider.to_string(),
            api_key: api_key.map(str::to_string),
            endpoint: endpoint.map(str::to_string),
        }
    }

    /// A stable hash of the connection, for the roster staleness check.
    ///
    /// Covers the key as well as the provider and endpoint, so rotating a key
    /// with everything else unchanged still rebuilds the roster — otherwise a
    /// rotated credential would keep authenticating with the old one until a
    /// restart.
    pub fn fingerprint(config: &Option<TenantSearch>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match config {
            None => 0u8.hash(&mut hasher),
            Some(search) => {
                1u8.hash(&mut hasher);
                search.provider.hash(&mut hasher);
                search.api_key.hash(&mut hasher);
                search.endpoint.hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

pub use live::{BYO_SEARCH_TOOLS, byo_search_tools};

mod live {
    use super::{DEFAULT_MAX_RESULTS, SEARXNG_LANGUAGE, TIMEOUT_SECS, TenantSearch};

    use async_trait::async_trait;
    use serde_json::Value;

    use oh::search::tools::{
        BraveImageSearchTool, BraveNewsSearchTool, BraveVideoSearchTool, BraveWebSearchTool,
        ExaFindSimilarTool, ExaGetContentsTool, ExaSearchTool, QueritSearchTool, SearxngSearchTool,
    };
    use oh::tools::traits::{
        PermissionLevel, Tool, ToolCallOptions, ToolCategory, ToolResult, ToolScope, ToolTimeout,
    };
    use openhuman_core::openhuman as oh;

    use crate::harness::search::WEB_SEARCH_TOOL;

    /// Every tool name a BYO provider can put on a belt, across all providers.
    ///
    /// Not "the names one company sees" — that is provider-dependent — but the
    /// closed set the `search` namespace has to account for, so
    /// [`namespace_of`](crate::harness::toolbelt::namespace_of) and the
    /// gateable-coverage invariant can be checked against it rather than against
    /// a list somebody remembered to update.
    pub const BYO_SEARCH_TOOLS: [&str; 6] = [
        "exa_find_similar",
        "exa_get_contents",
        "brave_news_search",
        "brave_image_search",
        "brave_video_search",
        WEB_SEARCH_TOOL,
    ];

    /// The search tools for one company's own provider connection.
    ///
    /// An unknown provider slug wires nothing and warns rather than failing the
    /// build: an agent that cannot search is a degraded agent, not a broken
    /// company. In practice the slug was validated by the console write route
    /// and again by [`TenantSearch::resolve`], so reaching the warn arm means
    /// somebody wrote the secret store directly.
    pub fn byo_search_tools(config: &TenantSearch) -> Vec<Box<dyn Tool>> {
        let key = config.api_key.clone();
        match config.provider.as_str() {
            "brave" => vec![
                alias(
                    BraveWebSearchTool::new(key.clone(), DEFAULT_MAX_RESULTS, TIMEOUT_SECS),
                    "Brave web search",
                ),
                Box::new(BraveNewsSearchTool::new(
                    key.clone(),
                    DEFAULT_MAX_RESULTS,
                    TIMEOUT_SECS,
                )),
                Box::new(BraveImageSearchTool::new(
                    key.clone(),
                    DEFAULT_MAX_RESULTS,
                    TIMEOUT_SECS,
                )),
                Box::new(BraveVideoSearchTool::new(
                    key,
                    DEFAULT_MAX_RESULTS,
                    TIMEOUT_SECS,
                )),
            ],
            "exa" => vec![
                alias(
                    ExaSearchTool::new(key.clone(), None, DEFAULT_MAX_RESULTS, TIMEOUT_SECS),
                    "Exa web search",
                ),
                Box::new(ExaFindSimilarTool::new(
                    key.clone(),
                    None,
                    DEFAULT_MAX_RESULTS,
                    TIMEOUT_SECS,
                )),
                Box::new(ExaGetContentsTool::new(
                    key,
                    None,
                    DEFAULT_MAX_RESULTS,
                    TIMEOUT_SECS,
                )),
            ],
            "querit" => vec![alias(
                QueritSearchTool::new(key, None, DEFAULT_MAX_RESULTS, TIMEOUT_SECS),
                "Querit web search",
            )],
            "searxng" => {
                // Resolution guarantees the endpoint for this provider; the
                // `unwrap_or_default` is the belt to that braces, and an empty
                // base URL makes the tool report an unreachable instance rather
                // than panic.
                let base_url = config.endpoint.clone().unwrap_or_default();
                vec![alias(
                    SearxngSearchTool::new(
                        base_url,
                        DEFAULT_MAX_RESULTS,
                        SEARXNG_LANGUAGE.to_string(),
                        TIMEOUT_SECS,
                    ),
                    "SearXNG web search",
                )]
            }
            other => {
                tracing::warn!(
                    provider = %other,
                    "[search] unknown BYO search provider stored; no search tools wired"
                );
                Vec::new()
            }
        }
    }

    /// Present `tool` to the model under OpenCompany's canonical
    /// [`WEB_SEARCH_TOOL`] name, with `label` as its operator-facing step
    /// label — the provider's name, since a BYO tool's engine is fixed at
    /// construction ("Exa web search"), matching the managed tool's branded
    /// label rather than the humanized alias name.
    fn alias(tool: impl Tool + 'static, label: &'static str) -> Box<dyn Tool> {
        Box::new(AliasedTool {
            inner: Box::new(tool),
            name: WEB_SEARCH_TOOL,
            label,
        })
    }

    /// One tool wearing a different name.
    ///
    /// Every other method delegates, so the aliased tool behaves exactly like
    /// the upstream one — including its schema, its permission level and its
    /// timeout policy. A method added to [`Tool`] upstream after this was
    /// written falls back to the trait default rather than the inner tool's
    /// override; the delegation list below is the thing to extend when that
    /// happens.
    struct AliasedTool {
        inner: Box<dyn Tool>,
        name: &'static str,
        label: &'static str,
    }

    #[async_trait]
    impl Tool for AliasedTool {
        fn name(&self) -> &str {
            self.name
        }

        // Not delegated: the inner tool's label names the upstream tool, and
        // the default would humanize the alias into a provider-less
        // "Web search".
        fn display_label(&self, _args: &Value) -> Option<String> {
            Some(self.label.to_string())
        }

        fn description(&self) -> &str {
            self.inner.description()
        }

        fn parameters_schema(&self) -> Value {
            self.inner.parameters_schema()
        }

        async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
            self.inner.execute(args).await
        }

        async fn execute_with_options(
            &self,
            args: Value,
            options: ToolCallOptions,
        ) -> anyhow::Result<ToolResult> {
            self.inner.execute_with_options(args, options).await
        }

        fn supports_markdown(&self) -> bool {
            self.inner.supports_markdown()
        }

        fn permission_level(&self) -> PermissionLevel {
            self.inner.permission_level()
        }

        fn permission_level_with_args(&self, args: &Value) -> PermissionLevel {
            self.inner.permission_level_with_args(args)
        }

        fn scope(&self) -> ToolScope {
            self.inner.scope()
        }

        fn category(&self) -> ToolCategory {
            self.inner.category()
        }

        fn is_concurrency_safe(&self, args: &Value) -> bool {
            self.inner.is_concurrency_safe(args)
        }

        fn external_effect(&self) -> bool {
            self.inner.external_effect()
        }

        fn external_effect_with_args(&self, args: &Value) -> bool {
            self.inner.external_effect_with_args(args)
        }

        fn max_result_size_chars(&self) -> Option<usize> {
            self.inner.max_result_size_chars()
        }

        fn timeout_policy(&self, args: &Value) -> ToolTimeout {
            self.inner.timeout_policy(args)
        }

        fn display_detail(&self, args: &Value) -> Option<String> {
            self.inner.display_detail(args)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::ports::types::SecretValue;

    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;

    #[derive(Default)]
    struct MemSecrets {
        map: Mutex<HashMap<String, String>>,
    }

    impl MemSecrets {
        fn with(pairs: &[(&str, &str)]) -> Arc<dyn SecretStore> {
            let store = MemSecrets::default();
            let mut map = store.map.lock().unwrap();
            for (key, value) in pairs {
                map.insert((*key).to_string(), (*value).to_string());
            }
            drop(map);
            Arc::new(store)
        }
    }

    #[async_trait]
    impl SecretStore for MemSecrets {
        async fn get(&self, _company: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(key)
                .map(|value| SecretValue(value.clone())))
        }
        async fn set(&self, _company: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), value.0);
            Ok(())
        }
    }

    /// A store whose reads always fail — the transient-hiccup case.
    struct BrokenSecrets;

    #[async_trait]
    impl SecretStore for BrokenSecrets {
        async fn get(&self, _company: &CompanyId, _key: &str) -> Result<Option<SecretValue>> {
            Err(crate::error::OpenCompanyError::Store("boom".into()))
        }
        async fn set(&self, _c: &CompanyId, _k: &str, _v: SecretValue) -> Result<()> {
            Err(crate::error::OpenCompanyError::Store("boom".into()))
        }
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    fn names(config: &TenantSearch) -> Vec<String> {
        byo_search_tools(config)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }

    /// The three states that must all mean "search through the managed
    /// surface", because each one is an ordinary state of a settings page and
    /// none of them should leave an agent unable to search.
    #[tokio::test]
    async fn nothing_configured_managed_and_a_missing_key_all_fall_back_to_managed() {
        let empty = MemSecrets::with(&[]);
        assert!(
            TenantSearch::resolve(&empty, &company())
                .await
                .unwrap()
                .is_none(),
            "an unconfigured company must fall back to managed"
        );

        let managed = MemSecrets::with(&[(PROVIDER_SECRET, "managed")]);
        assert!(
            TenantSearch::resolve(&managed, &company())
                .await
                .unwrap()
                .is_none(),
            "`managed` is the fallback, never a BYO connection"
        );

        let keyless = MemSecrets::with(&[(PROVIDER_SECRET, "brave")]);
        assert!(
            TenantSearch::resolve(&keyless, &company())
                .await
                .unwrap()
                .is_none(),
            "a BYO provider with no key must fall back rather than wire a keyless tool"
        );

        let unknown = MemSecrets::with(&[(PROVIDER_SECRET, "google"), (API_KEY_SECRET, "k")]);
        assert!(
            TenantSearch::resolve(&unknown, &company())
                .await
                .unwrap()
                .is_none(),
            "an unknown slug is not a connection"
        );
    }

    /// A store hiccup is an error, not "not configured": the two need opposite
    /// responses from the caller.
    #[tokio::test]
    async fn a_store_read_failure_is_an_error_and_not_a_silent_fallback() {
        let broken: Arc<dyn SecretStore> = Arc::new(BrokenSecrets);
        assert!(TenantSearch::resolve(&broken, &company()).await.is_err());
    }

    #[tokio::test]
    async fn searxng_resolves_on_an_endpoint_alone_and_not_on_a_key() {
        let key_only = MemSecrets::with(&[(PROVIDER_SECRET, "searxng"), (API_KEY_SECRET, "k")]);
        assert!(
            TenantSearch::resolve(&key_only, &company())
                .await
                .unwrap()
                .is_none(),
            "a SearXNG instance is addressed by URL; a key is not the missing half"
        );

        let with_endpoint = MemSecrets::with(&[
            (PROVIDER_SECRET, "searxng"),
            (ENDPOINT_SECRET, "https://searx.example"),
        ]);
        let resolved = TenantSearch::resolve(&with_endpoint, &company())
            .await
            .unwrap()
            .expect("an endpoint is the whole configuration for searxng");
        assert_eq!(resolved.provider(), "searxng");
    }

    /// Whichever provider is configured, the model is told about the same
    /// `web_search` — the affordance the shipped research skills name.
    #[test]
    fn every_provider_presents_its_canonical_tool_as_web_search() {
        for (provider, endpoint) in [
            ("brave", None),
            ("exa", None),
            ("querit", None),
            ("searxng", Some("https://searx.example".to_string())),
        ] {
            let config = TenantSearch {
                provider: provider.to_string(),
                api_key: Some("test-key".to_string()),
                endpoint,
            };
            let names = names(&config);
            assert!(
                names.contains(&crate::harness::search::WEB_SEARCH_TOOL.to_string()),
                "{provider} wired {names:?} with no canonical web_search"
            );
        }
    }

    /// The alias wears the canonical name but the step timeline names the
    /// engine — the humanized fallback would collapse every provider to a
    /// provider-less "Web search".
    #[test]
    fn the_aliased_tool_step_label_names_the_provider() {
        for (provider, endpoint, label) in [
            ("brave", None, "Brave web search"),
            ("exa", None, "Exa web search"),
            ("querit", None, "Querit web search"),
            (
                "searxng",
                Some("https://searx.example".to_string()),
                "SearXNG web search",
            ),
        ] {
            let config = TenantSearch {
                provider: provider.to_string(),
                api_key: Some("test-key".to_string()),
                endpoint,
            };
            let tools = byo_search_tools(&config);
            let aliased = tools
                .iter()
                .find(|tool| tool.name() == crate::harness::search::WEB_SEARCH_TOOL)
                .expect("every provider wires a canonical web_search");
            assert_eq!(
                aliased.display_label(&serde_json::json!({})).as_deref(),
                Some(label),
                "{provider}"
            );
            // And it reaches the operator. Every provider is aliased to the one
            // canonical `web_search`, so the tool *name* the loop labels from
            // carries no provider at all — only the tool's own label, restored by
            // `StepLabels`, can tell these four rows apart.
            assert_eq!(step_label(&tools), label, "{provider}");
        }
    }

    /// The label an operator actually reads for a `web_search` row, folded the
    /// way a real turn folds it: the loop supplies the humanized tool name,
    /// [`StepLabels`](crate::harness::steps::StepLabels) restores what the tool
    /// calls itself, and `fold_steps` renders the row.
    fn step_label(tools: &[Box<dyn openhuman_core::openhuman::tools::traits::Tool>]) -> String {
        use crate::harness::search::WEB_SEARCH_TOOL;
        use crate::harness::steps::{StepLabels, fold_steps};
        use openhuman_core::openhuman as oh;

        let labels = StepLabels::from_tools(tools);
        let started = oh::agent::progress::AgentProgress::ToolCallStarted {
            call_id: "c1".into(),
            tool_name: WEB_SEARCH_TOOL.into(),
            arguments: serde_json::Value::Null,
            iteration: 1,
            display_label: Some(oh::tools::traits::humanize_tool_name(WEB_SEARCH_TOOL)),
            display_detail: None,
        };
        fold_steps(vec![labels.apply(started)])
            .first()
            .expect("a tool call folds to one step")
            .label
            .clone()
    }

    #[test]
    fn the_provider_families_are_exactly_what_each_provider_supports() {
        let brave = TenantSearch {
            provider: "brave".to_string(),
            api_key: Some("k".to_string()),
            endpoint: None,
        };
        assert_eq!(
            names(&brave),
            vec![
                "web_search",
                "brave_news_search",
                "brave_image_search",
                "brave_video_search"
            ]
        );

        let exa = TenantSearch {
            provider: "exa".to_string(),
            api_key: Some("k".to_string()),
            endpoint: None,
        };
        assert_eq!(
            names(&exa),
            vec!["web_search", "exa_find_similar", "exa_get_contents"]
        );
    }

    /// Every name a provider family can put on a belt is accounted for by
    /// [`BYO_SEARCH_TOOLS`], so the namespace mapping cannot fall behind a
    /// provider somebody added.
    #[test]
    fn every_wired_name_is_declared_in_the_byo_tool_list() {
        for provider in ["brave", "exa", "querit", "searxng"] {
            let config = TenantSearch {
                provider: provider.to_string(),
                api_key: Some("k".to_string()),
                endpoint: Some("https://searx.example".to_string()),
            };
            for name in names(&config) {
                assert!(
                    BYO_SEARCH_TOOLS.contains(&name.as_str()),
                    "{provider} wires `{name}`, which BYO_SEARCH_TOOLS does not declare"
                );
            }
        }
    }

    /// An unknown slug that somehow reached the store wires nothing rather than
    /// panicking or wiring a tool with no provider behind it.
    #[test]
    fn an_unknown_provider_wires_nothing() {
        let config = TenantSearch {
            provider: "google".to_string(),
            api_key: Some("k".to_string()),
            endpoint: None,
        };
        assert!(names(&config).is_empty());
    }

    /// A rotated key must move the fingerprint, or the roster keeps
    /// authenticating with the credential the operator just replaced.
    #[test]
    fn the_fingerprint_moves_on_a_rotation_and_on_a_provider_switch() {
        let base = TenantSearch {
            provider: "exa".to_string(),
            api_key: Some("first".to_string()),
            endpoint: None,
        };
        let rotated = TenantSearch {
            api_key: Some("second".to_string()),
            ..base.clone()
        };
        let switched = TenantSearch {
            provider: "brave".to_string(),
            ..base.clone()
        };

        let fp = |config: &TenantSearch| TenantSearch::fingerprint(&Some(config.clone()));
        assert_ne!(fp(&base), fp(&rotated));
        assert_ne!(fp(&base), fp(&switched));
        assert_ne!(fp(&base), TenantSearch::fingerprint(&None));
        assert_eq!(fp(&base), fp(&base.clone()));
    }

    /// The key must never reach a log line.
    #[test]
    fn debug_redacts_the_key() {
        let rendered = format!(
            "{:?}",
            TenantSearch {
                provider: "exa".to_string(),
                api_key: Some("super-secret".to_string()),
                endpoint: None,
            }
        );
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
