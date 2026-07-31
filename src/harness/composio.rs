//! Per-tenant Composio tools (issue #110, epic #26 Cell D): the Gmail / Slack /
//! GitHub surface OpenHuman exposes through its backend-proxied Composio routes,
//! bridged into a company agent's toolbelt behind the opt-in `composio` grant.
//!
//! ## Tenant isolation (the security spine)
//!
//! In OpenHuman's **backend mode**, no Composio call carries an `entity_id`: the
//! backend derives the Composio entity from the **bearer JWT** on the request.
//! So the *only* isolation lever is **which credential the call is made with**,
//! and that resolution happens **server-side** here — never from agent input and
//! never from manifest free-text.
//!
//! Two sources, in strict precedence:
//!
//! 1. **The company's own token**, stored in its [`SecretStore`] under
//!    [`TOKEN_KEY`] by the console. A company that brings its own Composio
//!    identity keeps it, always.
//! 2. **This instance's platform identity** — the
//!    [`TinyhumansTokenSource`](crate::company::credentials::TinyhumansTokenSource)
//!    the runtime authenticates with everywhere else. On the hosted platform that
//!    is a projected, audience-bound token the cluster rotates in place, so it is
//!    read **per call**, never captured when the roster is built.
//!
//! With neither, resolution yields `None` and no tools are wired (fail closed) —
//! an absent credential must mean "no tools", never a borrowed identity. Two
//! companies pasting the *same* token would share one entity; that cannot be
//! prevented client-side and is documented as a deployment caveat.
//!
//! ## Rotation must not churn the roster
//!
//! The roster fingerprint hashes the credential's **identity**, not its bytes:
//! for the projected tier that is the tier + path (see
//! [`Credential::hash_identity`]). Hashing the value would rebuild every agent's
//! tool roster on the platform's rotation schedule — every few minutes, forever.
//! A pasted per-company token still fingerprints by value, because there a new
//! value really is a new identity.
//!
//! ## Write-only credential
//!
//! The token is write-only. Whatever value a call resolves is fed to [`scrub`] as
//! a known secret, so it cannot survive into **any** `ToolResult` (success or
//! error); it is absent from every tracing line and from the [`Debug`] impl. Only
//! non-secret status (backend URL, toolkit allowlist) is ever surfaced.

use std::sync::Arc;

use crate::company::credentials::{Credential, TinyhumansTokenSource};
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

// The credential key + backend routing live in the always-compiled
// `company::composio` module (so the console read/write plane can manage the
// token in the default build); re-exported here for the harness call sites.
pub use crate::company::composio::{
    COMPOSIO_BACKEND_URL_ENV, TINYHUMANS_API_URL_ENV, TOKEN_KEY, backend_url_or_default,
};

/// A per-tenant Composio configuration: the backend URL, how the outbound bearer
/// is obtained, and the toolkit allowlist.
///
/// **Security invariant**: the credential decides which Composio entity the
/// backend resolves, so a company can only ever reach its own connected
/// accounts. It is never logged, returned, or `Debug`-printed.
///
/// Always compiled (so the [`HarnessDeps`](crate::harness::HarnessDeps) field
/// exists in every `openhuman` build and every construction site fails closed
/// with `None`); the live tool constructors in [`composio_tools`] are gated
/// behind the `composio` feature.
#[derive(Clone, Debug)]
pub struct TenantComposio {
    /// The Composio backend base URL (e.g. `https://api.tinyhumans.ai`).
    pub backend_url: String,
    /// How the outbound bearer is obtained: the company's own stored token, or
    /// this instance's platform identity. Resolved per call — see the module docs.
    credential: Credential,
    /// The toolkit allowlist (Gmail / Slack / GitHub, …). Empty defers to the
    /// backend's server-enforced allowlist (open mode); non-empty narrows
    /// strictly, client-side, before any network round-trip.
    pub toolkits: Vec<String>,
}

impl TenantComposio {
    /// A config over an explicit credential — the constructor tests and callers
    /// outside the resolver use.
    pub fn new(
        backend_url: impl Into<String>,
        credential: Credential,
        toolkits: Vec<String>,
    ) -> Self {
        Self {
            backend_url: backend_url.into(),
            credential,
            toolkits,
        }
    }

    /// Resolve a per-tenant Composio config, or `None` (fail closed) when no
    /// credential can be obtained at all.
    ///
    /// Precedence: the company's **own** token under [`TOKEN_KEY`] in the secret
    /// store wins; otherwise this instance's platform identity (`token_source`)
    /// is used. A company that pasted its own Composio token therefore keeps it
    /// even on the hosted platform, and a company that pasted nothing borrows no
    /// one else's identity — it presents the identity of the instance it runs in,
    /// which the backend resolves to that instance's owner.
    ///
    /// The URL resolves from [`COMPOSIO_BACKEND_URL_ENV`], then the tenant API
    /// base [`TINYHUMANS_API_URL_ENV`], then [`DEFAULT_BACKEND_URL`] — see
    /// [`backend_url_or_default`]. `toolkits` is the manifest allowlist, threaded
    /// through unchanged.
    ///
    /// A secret-store read error degrades to the token source (and, failing that,
    /// to `None`) rather than bubbling — a transient store hiccup must not brick
    /// the roster build.
    pub async fn resolve(
        company: &CompanyId,
        secrets: &dyn SecretStore,
        toolkits: Vec<String>,
        backend_url_env: Option<String>,
        api_url_env: Option<String>,
        token_source: Option<Arc<TinyhumansTokenSource>>,
    ) -> Option<Self> {
        let stored = match secrets.get(company, TOKEN_KEY).await {
            Ok(Some(SecretValue(token))) => Credential::from_value(token),
            _ => Credential::None,
        };
        let credential = match (stored, token_source) {
            // The company's own token always wins.
            (stored @ Credential::Value(_), _) => stored,
            (_, Some(source)) => Credential::from_source(source),
            (_, None) => return None,
        };
        Some(Self::new(
            backend_url_or_default(backend_url_env, api_url_env),
            credential,
            toolkits,
        ))
    }

    /// The credential this config presents. Status and fingerprinting only —
    /// callers on the request path want [`Self::current_token`].
    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// The bearer to present on **this** Composio call.
    ///
    /// Resolved per call so a platform token the cluster rotated in place is
    /// picked up without rebuilding the roster. `None` would mean no credential
    /// at all, which [`Self::resolve`] already rules out; the tools refuse the
    /// call rather than dialling the backend unauthenticated.
    pub async fn current_token(&self) -> crate::Result<Option<String>> {
        self.credential.current().await
    }

    /// A stable, credential-safe fingerprint of the resolved config, folded into
    /// the harness roster fingerprint so a console token set/rotate (or a
    /// toolkit-allowlist change) rebuilds the roster on the next turn without a
    /// restart.
    ///
    /// The credential contributes its **identity**, not its bytes: a projected
    /// platform token rotates every few minutes, and hashing the value would
    /// rebuild the whole roster on that schedule. A pasted per-company token does
    /// contribute its value, since changing it is a real identity change.
    pub fn fingerprint(config: &Option<TenantComposio>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match config {
            None => 0u8.hash(&mut hasher),
            Some(c) => {
                1u8.hash(&mut hasher);
                c.backend_url.hash(&mut hasher);
                c.credential.hash_identity(&mut hasher);
                c.toolkits.hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

/// Whether a toolkit is admitted by an allowlist. An **empty** allowlist defers
/// to the backend's server-enforced allowlist (open mode) and admits every
/// toolkit; a non-empty allowlist admits only its members (case-insensitive).
fn toolkit_allowed(allowlist: &[String], toolkit: &str) -> bool {
    allowlist.is_empty()
        || allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(toolkit))
}

/// The toolkit a Composio action slug belongs to: the segment before the first
/// `_`, lowercased (`GMAIL_SEND_EMAIL` → `gmail`). Empty slug → empty string.
fn slug_toolkit(slug: &str) -> String {
    slug.split('_').next().unwrap_or("").to_ascii_lowercase()
}

#[cfg(feature = "composio")]
pub use live::{ComposioMetering, authorize_connect_url, composio_tools, list_connection_states};

#[cfg(feature = "composio")]
mod live {
    use super::*;

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use crate::harness::mcp_probe::scrub;
    use crate::metering::record_oauth_call;
    use crate::ports::UsageMeter;
    use crate::ports::now_millis;

    use oh::composio::ComposioClient;
    use oh::integrations::IntegrationClient;
    use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
    use openhuman_core::openhuman as oh;

    /// What `composio_execute` needs to meter a call it just made: the company
    /// the sample belongs to, the agent that made it, and the meter to write to.
    ///
    /// Bundled because these three travel together and are individually
    /// meaningless — and because [`composio_tools`] would otherwise grow four
    /// positional parameters at a call site that already has many.
    #[derive(Clone)]
    pub struct ComposioMetering {
        /// The company the sample is scoped to.
        pub company: CompanyId,
        /// The agent whose turn made the call.
        pub agent: String,
        /// The usage meter. `None` leaves metering off entirely (the harness
        /// wires no meter in some embeddings) — the tools still work.
        pub meter: Option<Arc<dyn UsageMeter>>,
    }

    /// Build the five per-tenant Composio tools over the tenant's credential.
    ///
    /// Each tool holds the shared [`TenantComposio`] and the toolkit allowlist,
    /// and builds its [`ComposioClient`] **when it runs** via [`live_call`] — the
    /// bearer is resolved then, not now, so a platform token that rotated since
    /// the roster was built still authenticates. The read tools are `ReadOnly`;
    /// the `authorize` / `execute` tools are `Execute` and additionally park for
    /// operator approval through the harness [`ApprovalPolicy`](crate::harness::policy).
    ///
    /// `metering` lets `composio_execute` record a
    /// [`SampleKind::OauthCall`](crate::ports::usage::SampleKind) sample per
    /// call it completes, which is what puts numbers in the Usage view's
    /// calls-by-provider chart (issue #152).
    ///
    /// Gated on the `composio` feature; the default/`openhuman` build never
    /// compiles this.
    pub fn composio_tools(
        config: &TenantComposio,
        metering: ComposioMetering,
    ) -> Vec<Box<dyn Tool>> {
        let config = Arc::new(config.clone());
        let toolkits = Arc::new(config.toolkits.clone());
        vec![
            Box::new(ComposioListToolkitsTool {
                config: Arc::clone(&config),
                toolkits: Arc::clone(&toolkits),
            }),
            Box::new(ComposioListConnectionsTool {
                config: Arc::clone(&config),
                toolkits: Arc::clone(&toolkits),
            }),
            Box::new(ComposioListToolsTool {
                config: Arc::clone(&config),
                toolkits: Arc::clone(&toolkits),
            }),
            Box::new(ComposioAuthorizeTool {
                config: Arc::clone(&config),
                toolkits: Arc::clone(&toolkits),
            }),
            Box::new(ComposioExecuteTool {
                config,
                toolkits,
                metering,
            }),
        ]
    }

    /// A client for one call plus the known-secret vector that call's output must
    /// be scrubbed against.
    ///
    /// Built per call on purpose. The Config-free seam
    /// `IntegrationClient::new(backend_url, auth_token)` takes the credential
    /// directly (no OpenHuman global `Config`), and the bearer it is handed is the
    /// ONLY isolation lever — see the module docs — so it must be the value the
    /// credential yields *now*. A per-call client costs one HTTP-client
    /// construction on a path that is already a network round-trip; capturing the
    /// token once instead would leave a hosted tenant presenting a bearer the
    /// cluster rotated away from minutes ago.
    ///
    /// The scrub vector carries exactly the token that went out, so it cannot
    /// survive into agent-visible output even if the backend reflects it.
    async fn live_call(config: &TenantComposio) -> Result<(ComposioClient, Vec<String>)> {
        let token = config
            .current_token()
            .await
            .map_err(|e| anyhow::anyhow!("resolving this company's Composio credential: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("no Composio credential is configured"))?;
        let client = ComposioClient::new(Arc::new(IntegrationClient::new(
            config.backend_url.clone(),
            token.clone(),
        )));
        Ok((client, vec![token]))
    }

    /// Serialize a successful response to JSON, then scrub it against the tenant
    /// token before it can reach the agent. Text-only output — the structured
    /// value is dropped so a credential the backend might reflect can never ride
    /// out in a JSON field.
    fn scrubbed_ok(value: Value, secrets: &[String]) -> ToolResult {
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        ToolResult::success(scrub(&text, secrets))
    }

    /// A scrubbed error result — the tenant token is stripped from any error
    /// body (mirrors [`crate::harness::mcp`]'s failure handling).
    fn scrubbed_err(context: &str, err: &anyhow::Error, secrets: &[String]) -> ToolResult {
        ToolResult::error(scrub(&format!("{context}: {err}"), secrets))
    }

    /// Pull a required, non-empty string argument.
    fn required_string_arg(args: &Value, key: &str) -> Result<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("missing required `{key}` string argument"))
    }

    /// Begin an OAuth handoff for `toolkit` and return the Composio-hosted
    /// connect URL the operator opens in a browser. Backs the console's
    /// `POST …/composio/authorize` route (the same building block the
    /// `composio_authorize` agent tool wraps).
    ///
    /// Composio runs the OAuth itself — there is **no** local callback route.
    /// The console opens the returned URL in a new tab and polls
    /// [`list_connection_states`] until the toolkit reports connected.
    ///
    /// The tenant allowlist is enforced **before** any network call — a toolkit
    /// the company is not permitted to connect never reaches the backend. Any
    /// upstream error is scrubbed of the tenant bearer before it bubbles.
    pub async fn authorize_connect_url(config: &TenantComposio, toolkit: &str) -> Result<String> {
        let toolkit = toolkit.trim();
        if toolkit.is_empty() {
            anyhow::bail!("composio authorize: toolkit must not be empty");
        }
        if !toolkit_allowed(&config.toolkits, toolkit) {
            anyhow::bail!("toolkit `{toolkit}` is not in this company's Composio allowlist");
        }
        tracing::debug!(toolkit = %toolkit, "[composio] ops authorize");
        let (client, secrets) = live_call(config).await?;
        match client.authorize(toolkit, None).await {
            Ok(resp) => Ok(resp.connect_url),
            Err(err) => Err(anyhow::anyhow!(scrub(&format!("{err}"), &secrets))),
        }
    }

    /// The per-toolkit connected state the console renders as provider rows: one
    /// `(toolkit, connected)` pair per toolkit that has at least one connection,
    /// with `connected == true` when **any** connection for that toolkit is
    /// active. Backs the console's `GET …/composio/connections` route.
    ///
    /// Filtered to the tenant allowlist (mirrors the `composio_list_connections`
    /// agent tool) so a connection outside the company's grant is never
    /// surfaced. Sorted by toolkit for a stable render order. Any upstream error
    /// is scrubbed of the tenant bearer before it bubbles.
    pub async fn list_connection_states(config: &TenantComposio) -> Result<Vec<(String, bool)>> {
        tracing::debug!(allowlist = ?config.toolkits, "[composio] ops list_connections");
        let (client, secrets) = live_call(config).await?;
        let resp = match client.list_connections().await {
            Ok(resp) => resp,
            Err(err) => return Err(anyhow::anyhow!(scrub(&format!("{err}"), &secrets))),
        };
        let mut states: std::collections::BTreeMap<String, bool> =
            std::collections::BTreeMap::new();
        for conn in resp.connections {
            let toolkit = conn.normalized_toolkit();
            if !toolkit_allowed(&config.toolkits, &toolkit) {
                continue;
            }
            let active = conn.is_active();
            states
                .entry(toolkit)
                .and_modify(|c| *c = *c || active)
                .or_insert(active);
        }
        Ok(states.into_iter().collect())
    }

    // ── composio_list_toolkits ──────────────────────────────────────────

    struct ComposioListToolkitsTool {
        config: Arc<TenantComposio>,
        toolkits: Arc<Vec<String>>,
    }

    #[async_trait]
    impl Tool for ComposioListToolkitsTool {
        fn name(&self) -> &str {
            "composio_list_toolkits"
        }

        fn description(&self) -> &str {
            "List the Composio toolkits (integrations such as Gmail, Slack, GitHub) available to this company. Read-only."
        }

        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::ReadOnly
        }

        async fn execute(&self, _args: Value) -> Result<ToolResult> {
            tracing::debug!(
                allowlist = ?self.toolkits,
                "[composio] list_toolkits"
            );
            let (client, secrets) = match live_call(&self.config).await {
                Ok(live) => live,
                Err(err) => {
                    return Ok(ToolResult::error(format!(
                        "composio_list_toolkits failed: {err}"
                    )));
                }
            };
            match client.list_toolkits().await {
                Ok(mut resp) => {
                    if !self.toolkits.is_empty() {
                        resp.toolkits
                            .retain(|slug| toolkit_allowed(&self.toolkits, slug));
                        resp.catalog
                            .retain(|entry| toolkit_allowed(&self.toolkits, &entry.slug));
                    }
                    Ok(scrubbed_ok(
                        serde_json::to_value(&resp).unwrap_or(Value::Null),
                        &secrets,
                    ))
                }
                Err(err) => Ok(scrubbed_err(
                    "composio_list_toolkits failed",
                    &err,
                    &secrets,
                )),
            }
        }
    }

    // ── composio_list_connections ───────────────────────────────────────

    struct ComposioListConnectionsTool {
        config: Arc<TenantComposio>,
        toolkits: Arc<Vec<String>>,
    }

    #[async_trait]
    impl Tool for ComposioListConnectionsTool {
        fn name(&self) -> &str {
            "composio_list_connections"
        }

        fn description(&self) -> &str {
            "List this company's connected Composio accounts (which Gmail / Slack / GitHub integrations are authorized). Read-only."
        }

        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::ReadOnly
        }

        async fn execute(&self, _args: Value) -> Result<ToolResult> {
            tracing::debug!(
                allowlist = ?self.toolkits,
                "[composio] list_connections"
            );
            let (client, secrets) = match live_call(&self.config).await {
                Ok(live) => live,
                Err(err) => {
                    return Ok(ToolResult::error(format!(
                        "composio_list_connections failed: {err}"
                    )));
                }
            };
            match client.list_connections().await {
                Ok(mut resp) => {
                    if !self.toolkits.is_empty() {
                        resp.connections.retain(|conn| {
                            toolkit_allowed(&self.toolkits, &conn.normalized_toolkit())
                        });
                    }
                    Ok(scrubbed_ok(
                        serde_json::to_value(&resp).unwrap_or(Value::Null),
                        &secrets,
                    ))
                }
                Err(err) => Ok(scrubbed_err(
                    "composio_list_connections failed",
                    &err,
                    &secrets,
                )),
            }
        }
    }

    // ── composio_list_tools ─────────────────────────────────────────────

    struct ComposioListToolsTool {
        config: Arc<TenantComposio>,
        toolkits: Arc<Vec<String>>,
    }

    #[async_trait]
    impl Tool for ComposioListToolsTool {
        fn name(&self) -> &str {
            "composio_list_tools"
        }

        fn description(&self) -> &str {
            "List the callable Composio actions (function schemas) for one or more toolkits. Pass `toolkits` (array of slugs) to narrow; omit to list every allowed toolkit's actions. Read-only."
        }

        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "toolkits": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Toolkit slugs to list actions for (e.g. `gmail`, `slack`, `github`)."
                    }
                },
                "additionalProperties": false
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::ReadOnly
        }

        async fn execute(&self, args: Value) -> Result<ToolResult> {
            // Requested toolkits (if any) intersected with the allowlist; when
            // the request is empty and an allowlist is set, use the allowlist as
            // the query so the backend never returns a toolkit the tenant is not
            // permitted to see.
            let requested: Vec<String> = args
                .get("toolkits")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();

            let effective: Vec<String> = if self.toolkits.is_empty() {
                requested
            } else if requested.is_empty() {
                self.toolkits.as_ref().clone()
            } else {
                requested
                    .into_iter()
                    .filter(|t| toolkit_allowed(&self.toolkits, t))
                    .collect()
            };
            tracing::debug!(
                effective = ?effective,
                allowlist = ?self.toolkits,
                "[composio] list_tools"
            );

            let query = if effective.is_empty() {
                None
            } else {
                Some(effective.as_slice())
            };
            let (client, secrets) = match live_call(&self.config).await {
                Ok(live) => live,
                Err(err) => {
                    return Ok(ToolResult::error(format!(
                        "composio_list_tools failed: {err}"
                    )));
                }
            };
            match client.list_tools(query, None).await {
                Ok(mut resp) => {
                    if !self.toolkits.is_empty() {
                        resp.tools.retain(|schema| {
                            toolkit_allowed(&self.toolkits, &slug_toolkit(&schema.function.name))
                        });
                    }
                    Ok(scrubbed_ok(
                        serde_json::to_value(&resp).unwrap_or(Value::Null),
                        &secrets,
                    ))
                }
                Err(err) => Ok(scrubbed_err("composio_list_tools failed", &err, &secrets)),
            }
        }
    }

    // ── composio_authorize ──────────────────────────────────────────────

    struct ComposioAuthorizeTool {
        config: Arc<TenantComposio>,
        toolkits: Arc<Vec<String>>,
    }

    #[async_trait]
    impl Tool for ComposioAuthorizeTool {
        fn name(&self) -> &str {
            "composio_authorize"
        }

        fn description(&self) -> &str {
            "Begin an OAuth handoff for a Composio toolkit (e.g. `gmail`) and return the hosted connect URL the operator opens in a browser to connect the account."
        }

        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "toolkit": {
                        "type": "string",
                        "description": "Toolkit slug to authorize (e.g. `gmail`, `slack`, `github`)."
                    },
                    "extra_params": {
                        "type": "object",
                        "description": "Optional extra fields some toolkits require during authorization."
                    }
                },
                "required": ["toolkit"],
                "additionalProperties": false
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::Execute
        }

        async fn execute(&self, args: Value) -> Result<ToolResult> {
            let toolkit = match required_string_arg(&args, "toolkit") {
                Ok(t) => t,
                Err(err) => return Ok(ToolResult::error(format!("composio_authorize: {err}"))),
            };
            // Enforce the allowlist BEFORE any network call — a toolkit the
            // tenant is not permitted to connect never reaches the backend.
            if !toolkit_allowed(&self.toolkits, &toolkit) {
                return Ok(ToolResult::error(format!(
                    "toolkit `{toolkit}` is not in this company's Composio allowlist"
                )));
            }
            let extra = args.get("extra_params").cloned();
            tracing::debug!(toolkit = %toolkit, "[composio] authorize");
            let (client, secrets) = match live_call(&self.config).await {
                Ok(live) => live,
                Err(err) => {
                    return Ok(ToolResult::error(format!(
                        "composio_authorize failed: {err}"
                    )));
                }
            };
            match client.authorize(&toolkit, extra).await {
                Ok(resp) => Ok(scrubbed_ok(
                    serde_json::to_value(&resp).unwrap_or(Value::Null),
                    &secrets,
                )),
                Err(err) => Ok(scrubbed_err("composio_authorize failed", &err, &secrets)),
            }
        }
    }

    // ── composio_execute ────────────────────────────────────────────────

    struct ComposioExecuteTool {
        config: Arc<TenantComposio>,
        toolkits: Arc<Vec<String>>,
        metering: ComposioMetering,
    }

    #[async_trait]
    impl Tool for ComposioExecuteTool {
        fn name(&self) -> &str {
            "composio_execute"
        }

        fn description(&self) -> &str {
            "Run a Composio action by its slug (e.g. `GMAIL_SEND_EMAIL`) with a JSON `arguments` object. Use `composio_list_tools` first to discover the slug and its parameters."
        }

        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "tool": {
                        "type": "string",
                        "description": "Composio action slug from `composio_list_tools` (e.g. `GMAIL_SEND_EMAIL`)."
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments object passed through to the Composio action."
                    }
                },
                "required": ["tool"],
                "additionalProperties": false
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::Execute
        }

        async fn execute(&self, args: Value) -> Result<ToolResult> {
            let tool = match required_string_arg(&args, "tool") {
                Ok(t) => t,
                Err(err) => return Ok(ToolResult::error(format!("composio_execute: {err}"))),
            };
            // Enforce the allowlist on the slug's toolkit prefix BEFORE any
            // network call (e.g. `GMAIL_SEND_EMAIL` → `gmail`).
            let toolkit = slug_toolkit(&tool);
            if !toolkit_allowed(&self.toolkits, &toolkit) {
                return Ok(ToolResult::error(format!(
                    "action `{tool}` targets toolkit `{toolkit}`, which is not in this company's Composio allowlist"
                )));
            }
            let arguments = args.get("arguments").cloned();
            // tracing carries the slug/toolkit only — NEVER arguments or bodies.
            tracing::debug!(tool = %tool, toolkit = %toolkit, "[composio] execute");
            let (client, secrets) = match live_call(&self.config).await {
                Ok(live) => live,
                Err(err) => {
                    return Ok(ToolResult::error(format!("composio_execute failed: {err}")));
                }
            };
            match client.execute_tool(&tool, arguments).await {
                Ok(resp) => {
                    // Metered only on success — i.e. a call that actually
                    // reached the connected account. `connections` in the read
                    // model is the *count of providers seen*, so counting a
                    // failed call would report a connection for a provider this
                    // company may not even be connected to. Never fails the
                    // call: `record_oauth_call` logs and swallows.
                    if let Some(meter) = self.metering.meter.as_deref() {
                        record_oauth_call(
                            meter,
                            &self.metering.company,
                            &self.metering.agent,
                            &toolkit,
                            now_millis(),
                        )
                        .await;
                    }
                    Ok(scrubbed_ok(
                        serde_json::to_value(&resp).unwrap_or(Value::Null),
                        &secrets,
                    ))
                }
                Err(err) => Ok(scrubbed_err("composio_execute failed", &err, &secrets)),
            }
        }
    }

    #[cfg(test)]
    mod live_tests {
        use super::*;

        use std::sync::Mutex;

        use crate::ports::types::CompanyId;
        use crate::ports::usage::UsageSample;

        #[derive(Default)]
        struct RecordingMeter {
            samples: Mutex<Vec<UsageSample>>,
        }

        #[async_trait]
        impl UsageMeter for RecordingMeter {
            async fn record(
                &self,
                _company: &CompanyId,
                sample: &UsageSample,
            ) -> crate::Result<()> {
                self.samples.lock().unwrap().push(sample.clone());
                Ok(())
            }
            async fn query(
                &self,
                _company: &CompanyId,
                _since: u64,
            ) -> crate::Result<Vec<UsageSample>> {
                Ok(self.samples.lock().unwrap().clone())
            }
        }

        /// An execute tool whose allowlist admits `gmail` only, wired to a
        /// recording meter. The client is constructed but never dialled by the
        /// paths under test — both return before any network call.
        fn tool_with(meter: Arc<RecordingMeter>) -> ComposioExecuteTool {
            ComposioExecuteTool {
                config: Arc::new(TenantComposio::new(
                    "https://example.invalid",
                    Credential::from_value("token"),
                    vec!["gmail".to_string()],
                )),
                toolkits: Arc::new(vec!["gmail".to_string()]),
                metering: ComposioMetering {
                    company: CompanyId::new("acme"),
                    agent: "ceo".to_string(),
                    meter: Some(meter),
                },
            }
        }

        /// A call blocked by the toolkit allowlist never reaches the provider,
        /// so it must not count towards `oauthCalls` — and must not invent a
        /// `connections` entry for a toolkit this company cannot even use.
        #[tokio::test]
        async fn allowlist_rejection_records_no_sample() {
            let meter = Arc::new(RecordingMeter::default());
            let result = tool_with(meter.clone())
                .execute(json!({"tool": "SLACK_POST_MESSAGE"}))
                .await
                .expect("execute returns a result rather than erroring");
            assert!(result.is_error, "the call should be refused");
            assert!(meter.samples.lock().unwrap().is_empty());
        }

        /// A malformed call is rejected before the client is touched, so it is
        /// likewise not a metered OAuth call.
        #[tokio::test]
        async fn missing_tool_argument_records_no_sample() {
            let meter = Arc::new(RecordingMeter::default());
            let result = tool_with(meter.clone())
                .execute(json!({}))
                .await
                .expect("execute returns a result rather than erroring");
            assert!(result.is_error, "the call should be refused");
            assert!(meter.samples.lock().unwrap().is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolkit_allowed_empty_defers_to_backend() {
        // Empty allowlist = open mode: every toolkit admitted.
        assert!(toolkit_allowed(&[], "gmail"));
        assert!(toolkit_allowed(&[], "anything"));
    }

    #[test]
    fn toolkit_allowed_non_empty_narrows_case_insensitively() {
        let allow = vec!["gmail".to_string(), "github".to_string()];
        assert!(toolkit_allowed(&allow, "gmail"));
        assert!(toolkit_allowed(&allow, "GMAIL"));
        assert!(toolkit_allowed(&allow, "GitHub"));
        assert!(!toolkit_allowed(&allow, "slack"));
    }

    #[test]
    fn slug_toolkit_extracts_lowercased_prefix() {
        assert_eq!(slug_toolkit("GMAIL_SEND_EMAIL"), "gmail");
        assert_eq!(slug_toolkit("SLACK_POST_MESSAGE"), "slack");
        assert_eq!(slug_toolkit("GITHUB_CREATE_ISSUE"), "github");
        assert_eq!(slug_toolkit(""), "");
    }

    #[test]
    fn debug_redacts_the_token() {
        let config = TenantComposio::new(
            "https://api.tinyhumans.ai",
            Credential::from_value("super-secret-tenant-token"),
            vec!["gmail".to_string()],
        );
        let shown = format!("{config:?}");
        assert!(
            !shown.contains("super-secret-tenant-token"),
            "token leaked: {shown}"
        );
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(shown.contains("api.tinyhumans.ai"), "{shown}");
        assert!(
            shown.contains("gmail"),
            "toolkits should be visible: {shown}"
        );
    }

    #[test]
    fn debug_marks_unset_token() {
        let config = TenantComposio::new("https://api.tinyhumans.ai", Credential::None, Vec::new());
        let shown = format!("{config:?}");
        assert!(shown.contains("<unset>"), "{shown}");
    }

    /// The resolver's precedence and its fail-closed floor: the company's own
    /// stored token always wins; with none stored the instance's platform token
    /// source is used; with neither there is no config at all (no tools) — never a
    /// borrowed identity. A raw `TINYHUMANS_API_KEY` in the environment is not a
    /// source: the platform identity is passed in explicitly by the caller.
    #[tokio::test]
    async fn resolve_prefers_the_stored_token_then_the_token_source_then_fails_closed() {
        use crate::ports::SecretStore;
        use crate::ports::types::{CompanyId, SecretValue};
        use crate::store::FsSecretStore;

        let dir =
            std::env::temp_dir().join(format!("oc-composio-res-{}", crate::ports::generate_id()));
        let secrets = FsSecretStore::new(&dir);
        let company = CompanyId::new("acme");
        let source = || Arc::new(TinyhumansTokenSource::static_key("platform-identity"));

        // Nothing stored and no platform identity → fail closed. That an ambient
        // `TINYHUMANS_API_KEY` cannot be consulted is guaranteed by the signature
        // — `resolve` takes the source explicitly and has no `EnvSource` — so it
        // needs no proof by process-env mutation. Setting one here used to leak
        // into every other test in this binary (`std::env` is process-wide and
        // nothing restored it), which made an ops-route assertion on
        // `credentialSource == "none"` flake depending on test order.
        assert!(
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, None)
                .await
                .is_none(),
            "no credential at all must fail closed"
        );

        // Nothing stored, but this instance has an identity → it is used.
        let attested =
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, Some(source()))
                .await
                .expect("the platform identity resolves");
        assert_eq!(
            token_of(&attested).await.as_deref(),
            Some("platform-identity")
        );

        // An explicitly-empty stored token is not a token: still the source.
        secrets
            .set(&company, TOKEN_KEY, SecretValue("   ".to_string()))
            .await
            .unwrap();
        let attested =
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, Some(source()))
                .await
                .expect("the platform identity resolves");
        assert_eq!(
            token_of(&attested).await.as_deref(),
            Some("platform-identity")
        );
        // …and with no source either, an empty stored token fails closed.
        assert!(
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, None)
                .await
                .is_none()
        );

        // The company's OWN token wins over the platform identity.
        secrets
            .set(
                &company,
                TOKEN_KEY,
                SecretValue("tenant-token-xyz".to_string()),
            )
            .await
            .unwrap();
        let resolved = TenantComposio::resolve(
            &company,
            &secrets,
            vec!["gmail".into()],
            None,
            None,
            Some(source()),
        )
        .await
        .expect("a stored token resolves");
        assert_eq!(
            token_of(&resolved).await.as_deref(),
            Some("tenant-token-xyz"),
            "a company that brought its own token keeps it"
        );
        assert_eq!(resolved.backend_url, "https://api.tinyhumans.ai");
        assert_eq!(resolved.toolkits, vec!["gmail".to_string()]);

        // With no explicit override, the tenant API base is threaded into the
        // backend URL so a staging tenant's Composio follows staging.
        let staged = TenantComposio::resolve(
            &company,
            &secrets,
            Vec::new(),
            None,
            Some("https://staging-api.tinyhumans.ai".into()),
            None,
        )
        .await
        .expect("a stored token resolves");
        assert_eq!(staged.backend_url, "https://staging-api.tinyhumans.ai");

        unsafe { std::env::remove_var("TINYHUMANS_API_KEY") };
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bearer a config would present right now.
    async fn token_of(config: &TenantComposio) -> Option<String> {
        config.current_token().await.expect("resolves")
    }

    fn config_with(credential: Credential) -> Option<TenantComposio> {
        Some(TenantComposio::new(
            "https://api.tinyhumans.ai",
            credential,
            vec!["gmail".to_string()],
        ))
    }

    /// The rotation contract: a projected platform token whose bytes change every
    /// few minutes must NOT move the roster fingerprint, or every agent's tool
    /// roster is rebuilt on the cluster's rotation schedule. A tier change or a
    /// changed *stored* token must still move it.
    #[tokio::test]
    async fn fingerprint_is_stable_across_a_projected_rotation() {
        let dir =
            std::env::temp_dir().join(format!("oc-composio-fp-{}", crate::ports::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "token-before").unwrap();

        let projected = config_with(Credential::from_source(Arc::new(
            TinyhumansTokenSource::projected_file(&path),
        )));
        let before = TenantComposio::fingerprint(&projected);
        assert_eq!(
            token_of(projected.as_ref().unwrap()).await.as_deref(),
            Some("token-before")
        );

        // The kubelet rewrites the file in place.
        std::fs::write(&path, "token-after").unwrap();
        assert_eq!(
            token_of(projected.as_ref().unwrap()).await.as_deref(),
            Some("token-after"),
            "the call must pick up the rotated token"
        );
        assert_eq!(
            TenantComposio::fingerprint(&projected),
            before,
            "a rotation must NOT rebuild the roster"
        );

        // A different projected path is a different identity.
        let other = config_with(Credential::from_source(Arc::new(
            TinyhumansTokenSource::projected_file(dir.join("other")),
        )));
        assert_ne!(TenantComposio::fingerprint(&other), before);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fingerprint_moves_on_tier_and_stored_value_changes() {
        let a = config_with(Credential::from_value("token-a"));
        let b = config_with(Credential::from_value("token-b"));
        assert_ne!(
            TenantComposio::fingerprint(&a),
            TenantComposio::fingerprint(&b),
            "a rotated stored token must move the fingerprint"
        );
        assert_ne!(
            TenantComposio::fingerprint(&a),
            TenantComposio::fingerprint(&None),
            "None (fail-closed) must differ from a configured tenant"
        );
        assert_eq!(
            TenantComposio::fingerprint(&a),
            TenantComposio::fingerprint(&a.clone()),
            "the same config fingerprints stably"
        );

        // Swapping a stored token for the platform identity is a tier change.
        let attested = config_with(Credential::from_source(Arc::new(
            TinyhumansTokenSource::projected_file("/var/run/secrets/tinyhumans.ai/token"),
        )));
        assert_ne!(
            TenantComposio::fingerprint(&attested),
            TenantComposio::fingerprint(&a),
            "a tier change must move the fingerprint"
        );
    }
}

/// The console-facing ops helpers ([`authorize_connect_url`],
/// [`list_connection_states`]) over a mock Composio backend: proves the connect
/// URL is surfaced, the allowlist is enforced before any network call, and
/// connection rows aggregate to per-toolkit `connected` state filtered to the
/// tenant grant.
#[cfg(all(test, feature = "composio"))]
mod ops_helper_tests {
    use super::*;

    use std::net::SocketAddr;

    use axum::Router;
    use axum::routing::{get, post};
    use serde_json::{Value, json};

    /// Mock `POST /agent-integrations/composio/authorize` — returns a hosted
    /// connect URL inside the backend's `{success,data}` envelope.
    async fn authorize_handler() -> axum::Json<Value> {
        axum::Json(json!({
            "success": true,
            "data": { "connectUrl": "https://connect.composio.dev/abc", "connectionId": "conn-xyz" }
        }))
    }

    /// Mock `GET /agent-integrations/composio/connections` — gmail has one
    /// active + one pending row (→ connected), slack only pending (→ not
    /// connected), notion active (filtered out unless allowlisted).
    async fn connections_handler() -> axum::Json<Value> {
        axum::Json(json!({
            "success": true,
            "data": { "connections": [
                { "id": "c1", "toolkit": "gmail", "status": "ACTIVE" },
                { "id": "c2", "toolkit": "gmail", "status": "INITIATED" },
                { "id": "c3", "toolkit": "slack", "status": "INITIATED" },
                { "id": "c4", "toolkit": "notion", "status": "ACTIVE" }
            ] }
        }))
    }

    async fn spawn_backend() -> String {
        let app = Router::new()
            .route(
                "/agent-integrations/composio/authorize",
                post(authorize_handler),
            )
            .route(
                "/agent-integrations/composio/connections",
                get(connections_handler),
            );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn config(url: &str, toolkits: Vec<String>) -> TenantComposio {
        TenantComposio::new(
            url.to_string(),
            Credential::from_value("tenant-token"),
            toolkits,
        )
    }

    #[tokio::test]
    async fn authorize_returns_hosted_connect_url() {
        let url = spawn_backend().await;
        let out = authorize_connect_url(&config(&url, vec!["gmail".into()]), "gmail")
            .await
            .expect("authorize returns a connect URL");
        assert_eq!(out, "https://connect.composio.dev/abc");
    }

    #[tokio::test]
    async fn authorize_rejects_toolkit_outside_allowlist_before_any_network_call() {
        // Backend URL is unreachable — the allowlist rejection must fire first.
        let out =
            authorize_connect_url(&config("http://127.0.0.1:1", vec!["gmail".into()]), "slack")
                .await;
        let err = out.expect_err("a toolkit outside the allowlist must be refused");
        assert!(err.to_string().contains("allowlist"), "{err}");
    }

    #[tokio::test]
    async fn list_connection_states_aggregates_active_and_filters_to_allowlist() {
        let url = spawn_backend().await;
        // gmail + slack allowed; notion is active upstream but not in the grant.
        let states = list_connection_states(&config(&url, vec!["gmail".into(), "slack".into()]))
            .await
            .expect("list connections");
        assert_eq!(
            states,
            vec![("gmail".to_string(), true), ("slack".to_string(), false)],
            "gmail active (one ACTIVE row), slack pending only, notion filtered out"
        );
    }

    #[tokio::test]
    async fn list_connection_states_empty_allowlist_admits_every_toolkit() {
        let url = spawn_backend().await;
        let states = list_connection_states(&config(&url, Vec::new()))
            .await
            .expect("list connections");
        assert_eq!(
            states,
            vec![
                ("gmail".to_string(), true),
                ("notion".to_string(), true),
                ("slack".to_string(), false),
            ]
        );
    }
}

/// The mandatory tenant-isolation test (issue #110): two per-tenant configs (A
/// and B) over a mock backend that records the `Authorization` header of each
/// request and answers with tenant-specific data. Proves the ONLY isolation
/// lever — which token the client is constructed with — actually holds: A's
/// request carries token A (never B), and A's result carries only A's account.
#[cfg(all(test, feature = "composio"))]
mod isolation_tests {
    use super::*;

    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use oh::tools::traits::Tool;
    use openhuman_core::openhuman as oh;
    use serde_json::{Value, json};

    /// Shared recorder for every `Authorization` header the mock backend saw.
    type AuthLog = Arc<Mutex<Vec<String>>>;

    /// The mock `/agent-integrations/composio/connections` handler: records the
    /// bearer it received and returns a `{success,data}` envelope whose single
    /// connection's `account_email` is derived from that bearer — so a caller can
    /// prove it only ever sees *its own* tenant's data.
    async fn connections(State(log): State<AuthLog>, headers: HeaderMap) -> axum::Json<Value> {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        log.lock().unwrap().push(auth.clone());
        // Derive tenant identity purely from the presented bearer.
        let email = if auth.contains("token-a") {
            "a@example.com"
        } else if auth.contains("token-b") {
            "b@example.com"
        } else {
            "unknown@example.com"
        };
        axum::Json(json!({
            "success": true,
            "data": {
                "connections": [
                    { "id": "conn-1", "toolkit": "gmail", "status": "ACTIVE", "accountEmail": email }
                ]
            }
        }))
    }

    /// Spawn the mock backend on an ephemeral port; returns its base URL + the
    /// auth-header recorder.
    async fn spawn_backend() -> (String, AuthLog) {
        let log: AuthLog = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/agent-integrations/composio/connections", get(connections))
            .with_state(log.clone());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), log)
    }

    fn config(url: &str, token: &str) -> TenantComposio {
        TenantComposio::new(url, Credential::from_value(token), Vec::new())
    }

    fn list_connections_tool(config: &TenantComposio) -> Box<dyn Tool> {
        let metering = ComposioMetering {
            company: CompanyId::new("acme"),
            agent: "ceo".to_string(),
            meter: None,
        };
        composio_tools(config, metering)
            .into_iter()
            .find(|t| t.name() == "composio_list_connections")
            .expect("composio_list_connections tool present")
    }

    #[tokio::test]
    async fn each_tenant_only_ever_carries_its_own_token_and_sees_its_own_accounts() {
        let (url, log) = spawn_backend().await;

        let tool_a = list_connections_tool(&config(&url, "token-a"));
        let tool_b = list_connections_tool(&config(&url, "token-b"));

        let out_a = tool_a.execute(json!({})).await.unwrap();
        let text_a = out_a.output();
        let out_b = tool_b.execute(json!({})).await.unwrap();
        let text_b = out_b.output();

        // A saw only A's account; never B's account nor B's token.
        assert!(
            text_a.contains("a@example.com"),
            "A missing its account: {text_a}"
        );
        assert!(
            !text_a.contains("b@example.com"),
            "A leaked B's account: {text_a}"
        );
        assert!(!text_a.contains("token-b"), "A leaked B's token: {text_a}");
        // Symmetrically for B.
        assert!(
            text_b.contains("b@example.com"),
            "B missing its account: {text_b}"
        );
        assert!(
            !text_b.contains("a@example.com"),
            "B leaked A's account: {text_b}"
        );

        // A's own token is scrubbed out of its own successful output.
        assert!(
            !text_a.contains("token-a"),
            "A leaked its own token: {text_a}"
        );

        // The backend received exactly the two distinct bearers — each request
        // carried its own tenant's token, never the other's.
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "expected one request per tenant: {seen:?}");
        assert!(
            seen.iter().any(|a| a == "Bearer token-a"),
            "missing A bearer: {seen:?}"
        );
        assert!(
            seen.iter().any(|a| a == "Bearer token-b"),
            "missing B bearer: {seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|a| a.contains("token-a") && a.contains("token-b")),
            "a single request must never carry both tokens: {seen:?}"
        );
    }

    /// The rotation contract at the tool boundary: a projected platform token the
    /// cluster rewrites in place must reach the backend on the **next** call, with
    /// no roster rebuild — and the freshly-resolved value must be the one the
    /// scrub vector protects, so a backend that reflects it still cannot leak it.
    #[tokio::test]
    async fn a_rotated_projected_token_is_presented_and_scrubbed_per_call() {
        use crate::company::credentials::TinyhumansTokenSource;

        // Reflect the bearer back inside an envelope failure, and record it.
        async fn reflect(State(log): State<AuthLog>, headers: HeaderMap) -> axum::Json<Value> {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            log.lock().unwrap().push(auth.clone());
            axum::Json(json!({ "success": false, "error": format!("upstream said: {auth}") }))
        }
        let log: AuthLog = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/agent-integrations/composio/connections", get(reflect))
            .with_state(log.clone());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir =
            std::env::temp_dir().join(format!("oc-composio-rot-{}", crate::ports::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "projected-secret-before").unwrap();

        // ONE config, built once — exactly what a roster holds across turns.
        let config = TenantComposio::new(
            format!("http://{addr}"),
            Credential::from_source(Arc::new(TinyhumansTokenSource::projected_file(&path))),
            Vec::new(),
        );
        let tool = list_connections_tool(&config);

        let first = tool.execute(json!({})).await.unwrap();
        assert!(
            !first.output().contains("projected-secret-before"),
            "the resolved token leaked into agent-visible output: {}",
            first.output()
        );

        // The kubelet rewrites the file in place; the SAME tool must present the
        // new token and scrub that one.
        std::fs::write(&path, "projected-secret-after").unwrap();
        let second = tool.execute(json!({})).await.unwrap();
        assert!(
            !second.output().contains("projected-secret-after"),
            "the rotated token leaked into agent-visible output: {}",
            second.output()
        );

        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                "Bearer projected-secret-before".to_string(),
                "Bearer projected-secret-after".to_string()
            ],
            "each call must carry the token the file held at that moment: {seen:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A mock backend that echoes the caller's bearer inside an error body; the
    /// tool's scrub must strip it before the agent ever sees it.
    #[tokio::test]
    async fn error_body_reflecting_the_token_is_scrubbed() {
        async fn reflect(headers: HeaderMap) -> axum::Json<Value> {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            // A 2xx envelope failure whose message reflects the raw bearer.
            axum::Json(json!({ "success": false, "error": format!("upstream said: {auth}") }))
        }
        let app = Router::new().route("/agent-integrations/composio/connections", get(reflect));
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("http://{addr}");

        let tool = list_connections_tool(&config(&url, "reflected-secret-token"));
        let out = tool.execute(json!({})).await.unwrap();
        let text = out.output();
        assert!(
            !text.contains("reflected-secret-token"),
            "the reflected token leaked into agent-visible output: {text}"
        );
    }
}
