//! Per-tenant Composio tools (issue #110, epic #26 Cell D): the Gmail / Slack /
//! GitHub surface OpenHuman exposes through its backend-proxied Composio routes,
//! bridged into a company agent's toolbelt behind the opt-in `composio` grant.
//!
//! ## Tenant isolation (the security spine)
//!
//! In OpenHuman's **backend mode**, no Composio call carries an `entity_id`: the
//! backend derives the Composio entity from the **bearer JWT** on the request.
//! So the *only* isolation lever is **which token the client is constructed
//! with**, and that construction happens **server-side** here — never from agent
//! input and never from manifest free-text. The token is resolved from the
//! company's [`SecretStore`] under [`TOKEN_KEY`]; it has **no environment
//! fallback**, so a missing/empty secret yields `None` and no tools are wired
//! (fail closed). Two companies pasting the *same* token would share one entity
//! — that cannot be prevented client-side and is documented as a deployment
//! caveat (one backend token per company).
//!
//! ## Write-only token
//!
//! The token is a write-only credential. It is scrubbed out of **every**
//! `ToolResult` (success and error), every tracing line, and the [`Debug`]
//! impl. Only non-secret status (backend URL, toolkit allowlist) is ever
//! surfaced.

use std::sync::Arc;

use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

// The credential key + backend routing live in the always-compiled
// `company::composio` module (so the console read/write plane can manage the
// token in the default build); re-exported here for the harness call sites.
pub use crate::company::composio::{
    COMPOSIO_BACKEND_URL_ENV, TINYHUMANS_API_URL_ENV, TOKEN_KEY, backend_url_or_default,
};

/// A per-tenant Composio configuration: the backend URL, the tenant's OAuth
/// bearer token, and the toolkit allowlist.
///
/// **Security invariant**: `auth_token` is a per-company credential. The backend
/// derives the Composio entity from it, so a company can only ever reach its own
/// connected accounts. The token is never logged, returned, or `Debug`-printed.
///
/// Always compiled (so the [`HarnessDeps`](crate::harness::HarnessDeps) field
/// exists in every `openhuman` build and every construction site fails closed
/// with `None`); the live tool constructors in [`composio_tools`] are gated
/// behind the `composio` feature.
#[derive(Clone)]
pub struct TenantComposio {
    /// The Composio backend base URL (e.g. `https://api.tinyhumans.ai`).
    pub backend_url: String,
    /// The per-tenant OAuth bearer token. Never a managed/platform key; the
    /// backend derives the tenant's Composio entity from it. Write-only.
    pub auth_token: String,
    /// The toolkit allowlist (Gmail / Slack / GitHub, …). Empty defers to the
    /// backend's server-enforced allowlist (open mode); non-empty narrows
    /// strictly, client-side, before any network round-trip.
    pub toolkits: Vec<String>,
}

impl std::fmt::Debug for TenantComposio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let the tenant credential land in a trace.
        f.debug_struct("TenantComposio")
            .field("backend_url", &self.backend_url)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .field("toolkits", &self.toolkits)
            .finish()
    }
}

impl TenantComposio {
    /// Resolve a per-tenant Composio config from the company secret store, or
    /// `None` (fail closed) when no non-empty token is stored.
    ///
    /// The token comes **only** from [`TOKEN_KEY`] in the secret store — there is
    /// no environment or platform-key fallback, by design: an absent token must
    /// mean "no tools", never "borrow someone else's identity". The URL resolves
    /// from [`COMPOSIO_BACKEND_URL_ENV`], then the tenant API base
    /// [`TINYHUMANS_API_URL_ENV`], then [`DEFAULT_BACKEND_URL`] — see
    /// [`backend_url_or_default`]. `toolkits` is the manifest allowlist, threaded
    /// through unchanged.
    ///
    /// A secret-store read error degrades to `None` (fail closed) rather than
    /// bubbling — a transient store hiccup withholds the tools for that turn
    /// instead of bricking the roster build.
    pub async fn resolve(
        company: &CompanyId,
        secrets: &dyn SecretStore,
        toolkits: Vec<String>,
        backend_url_env: Option<String>,
        api_url_env: Option<String>,
    ) -> Option<Self> {
        let token = match secrets.get(company, TOKEN_KEY).await {
            Ok(Some(SecretValue(token))) => token,
            _ => return None,
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return None;
        }
        Some(Self {
            backend_url: backend_url_or_default(backend_url_env, api_url_env),
            auth_token: token,
            toolkits,
        })
    }

    /// A stable, credential-safe fingerprint of the resolved config, folded into
    /// the harness roster fingerprint so a console token set/rotate (or a
    /// toolkit-allowlist change) rebuilds the roster on the next turn without a
    /// restart. Hashes the token too so a rotation is detected — but the hash is
    /// one-way, so the fingerprint never carries the secret.
    pub fn fingerprint(config: &Option<TenantComposio>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match config {
            None => 0u8.hash(&mut hasher),
            Some(c) => {
                1u8.hash(&mut hasher);
                c.backend_url.hash(&mut hasher);
                c.auth_token.hash(&mut hasher);
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

    /// Build the five per-tenant Composio tools over the tenant's bearer token.
    ///
    /// Each tool holds its own [`ComposioClient`] (cheap `Arc` clone over the
    /// shared [`IntegrationClient`]), the known-secret vector `[auth_token]` fed
    /// to [`scrub`] so the credential can never survive into agent-visible
    /// output, and the toolkit allowlist. The read tools are `ReadOnly`; the
    /// `authorize` / `execute` tools are `Execute` and additionally park for
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
        // The Config-free seam: `IntegrationClient::new(backend_url, auth_token)`
        // takes the per-tenant credential directly, with no OpenHuman global
        // `Config`. The bearer is the ONLY isolation lever — see the module docs.
        let client = client_for(config);
        let secrets = vec![config.auth_token.clone()];
        let toolkits = Arc::new(config.toolkits.clone());
        vec![
            Box::new(ComposioListToolkitsTool {
                client: client.clone(),
                secrets: secrets.clone(),
                toolkits: Arc::clone(&toolkits),
            }),
            Box::new(ComposioListConnectionsTool {
                client: client.clone(),
                secrets: secrets.clone(),
                toolkits: Arc::clone(&toolkits),
            }),
            Box::new(ComposioListToolsTool {
                client: client.clone(),
                secrets: secrets.clone(),
                toolkits: Arc::clone(&toolkits),
            }),
            Box::new(ComposioAuthorizeTool {
                client: client.clone(),
                secrets: secrets.clone(),
                toolkits: Arc::clone(&toolkits),
            }),
            Box::new(ComposioExecuteTool {
                client,
                secrets,
                toolkits,
                metering,
            }),
        ]
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

    /// Build a [`ComposioClient`] over a resolved tenant config.
    ///
    /// The Config-free seam: `IntegrationClient::new(backend_url, auth_token)`
    /// takes the per-tenant credential directly, with no OpenHuman global
    /// `Config`. The bearer is the ONLY isolation lever — see the module docs.
    /// Shared by the agent tools ([`composio_tools`]) and the operator-console
    /// ops handlers ([`authorize_connect_url`], [`list_connection_states`]) so
    /// both dial the backend the exact same way.
    fn client_for(config: &TenantComposio) -> ComposioClient {
        ComposioClient::new(Arc::new(IntegrationClient::new(
            config.backend_url.clone(),
            config.auth_token.clone(),
        )))
    }

    /// Scrub the tenant bearer out of an upstream error before it can bubble to
    /// a console handler or a log line — the backend can reflect the presented
    /// bearer in an error body, and the ops surface must never echo it.
    fn scrub_err(err: &anyhow::Error, config: &TenantComposio) -> anyhow::Error {
        anyhow::anyhow!(scrub(&format!("{err}"), &[config.auth_token.clone()]))
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
        match client_for(config).authorize(toolkit, None).await {
            Ok(resp) => Ok(resp.connect_url),
            Err(err) => Err(scrub_err(&err, config)),
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
        let resp = match client_for(config).list_connections().await {
            Ok(resp) => resp,
            Err(err) => return Err(scrub_err(&err, config)),
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
        client: ComposioClient,
        secrets: Vec<String>,
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
            match self.client.list_toolkits().await {
                Ok(mut resp) => {
                    if !self.toolkits.is_empty() {
                        resp.toolkits
                            .retain(|slug| toolkit_allowed(&self.toolkits, slug));
                        resp.catalog
                            .retain(|entry| toolkit_allowed(&self.toolkits, &entry.slug));
                    }
                    Ok(scrubbed_ok(
                        serde_json::to_value(&resp).unwrap_or(Value::Null),
                        &self.secrets,
                    ))
                }
                Err(err) => Ok(scrubbed_err(
                    "composio_list_toolkits failed",
                    &err,
                    &self.secrets,
                )),
            }
        }
    }

    // ── composio_list_connections ───────────────────────────────────────

    struct ComposioListConnectionsTool {
        client: ComposioClient,
        secrets: Vec<String>,
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
            match self.client.list_connections().await {
                Ok(mut resp) => {
                    if !self.toolkits.is_empty() {
                        resp.connections.retain(|conn| {
                            toolkit_allowed(&self.toolkits, &conn.normalized_toolkit())
                        });
                    }
                    Ok(scrubbed_ok(
                        serde_json::to_value(&resp).unwrap_or(Value::Null),
                        &self.secrets,
                    ))
                }
                Err(err) => Ok(scrubbed_err(
                    "composio_list_connections failed",
                    &err,
                    &self.secrets,
                )),
            }
        }
    }

    // ── composio_list_tools ─────────────────────────────────────────────

    struct ComposioListToolsTool {
        client: ComposioClient,
        secrets: Vec<String>,
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
            match self.client.list_tools(query, None).await {
                Ok(mut resp) => {
                    if !self.toolkits.is_empty() {
                        resp.tools.retain(|schema| {
                            toolkit_allowed(&self.toolkits, &slug_toolkit(&schema.function.name))
                        });
                    }
                    Ok(scrubbed_ok(
                        serde_json::to_value(&resp).unwrap_or(Value::Null),
                        &self.secrets,
                    ))
                }
                Err(err) => Ok(scrubbed_err(
                    "composio_list_tools failed",
                    &err,
                    &self.secrets,
                )),
            }
        }
    }

    // ── composio_authorize ──────────────────────────────────────────────

    struct ComposioAuthorizeTool {
        client: ComposioClient,
        secrets: Vec<String>,
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
                Err(err) => return Ok(scrubbed_err("composio_authorize", &err, &self.secrets)),
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
            match self.client.authorize(&toolkit, extra).await {
                Ok(resp) => Ok(scrubbed_ok(
                    serde_json::to_value(&resp).unwrap_or(Value::Null),
                    &self.secrets,
                )),
                Err(err) => Ok(scrubbed_err(
                    "composio_authorize failed",
                    &err,
                    &self.secrets,
                )),
            }
        }
    }

    // ── composio_execute ────────────────────────────────────────────────

    struct ComposioExecuteTool {
        client: ComposioClient,
        secrets: Vec<String>,
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
                Err(err) => return Ok(scrubbed_err("composio_execute", &err, &self.secrets)),
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
            match self.client.execute_tool(&tool, arguments).await {
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
                        &self.secrets,
                    ))
                }
                Err(err) => Ok(scrubbed_err("composio_execute failed", &err, &self.secrets)),
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
                client: ComposioClient::new(Arc::new(IntegrationClient::new(
                    "https://example.invalid".to_string(),
                    "token".to_string(),
                ))),
                secrets: vec!["token".to_string()],
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
        let config = TenantComposio {
            backend_url: "https://api.tinyhumans.ai".to_string(),
            auth_token: "super-secret-tenant-token".to_string(),
            toolkits: vec!["gmail".to_string()],
        };
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
        let config = TenantComposio {
            backend_url: "https://api.tinyhumans.ai".to_string(),
            auth_token: String::new(),
            toolkits: Vec::new(),
        };
        let shown = format!("{config:?}");
        assert!(shown.contains("<unset>"), "{shown}");
    }

    /// Tenant isolation at the resolver seam (issue #110): the token comes ONLY
    /// from the company secret store — there is no env/platform-key fallback — so
    /// a company with no stored token resolves to `None` (fail closed, no tools),
    /// never a borrowed identity, even if a platform key like `TINYHUMANS_API_KEY`
    /// exists in the environment. A stored token resolves to a config that
    /// carries exactly that token.
    #[tokio::test]
    async fn resolve_is_fail_closed_without_a_stored_token_and_has_no_env_fallback() {
        use crate::ports::SecretStore;
        use crate::ports::types::{CompanyId, SecretValue};
        use crate::store::FsSecretStore;

        let dir =
            std::env::temp_dir().join(format!("oc-composio-res-{}", crate::ports::generate_id()));
        let secrets = FsSecretStore::new(&dir);
        let company = CompanyId::new("acme");

        // A platform key in the environment must NOT leak in — the resolver never
        // reads it. (No token stored → None.)
        // SAFETY: single-threaded test; we set then restore the env.
        unsafe { std::env::set_var("TINYHUMANS_API_KEY", "platform-managed-key") };
        assert!(
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None)
                .await
                .is_none(),
            "no stored token must fail closed, ignoring any env platform key"
        );

        // An explicitly-empty token still fails closed.
        secrets
            .set(&company, TOKEN_KEY, SecretValue("   ".to_string()))
            .await
            .unwrap();
        assert!(
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None)
                .await
                .is_none(),
            "an empty/whitespace token must fail closed"
        );

        // A real stored token resolves and carries exactly that token + default URL.
        secrets
            .set(
                &company,
                TOKEN_KEY,
                SecretValue("tenant-token-xyz".to_string()),
            )
            .await
            .unwrap();
        let resolved =
            TenantComposio::resolve(&company, &secrets, vec!["gmail".into()], None, None)
                .await
                .expect("a stored token resolves");
        assert_eq!(resolved.auth_token, "tenant-token-xyz");
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
        )
        .await
        .expect("a stored token resolves");
        assert_eq!(staged.backend_url, "https://staging-api.tinyhumans.ai");

        unsafe { std::env::remove_var("TINYHUMANS_API_KEY") };
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fingerprint_changes_on_token_rotation_and_none() {
        let a = Some(TenantComposio {
            backend_url: "https://api.tinyhumans.ai".to_string(),
            auth_token: "token-a".to_string(),
            toolkits: vec!["gmail".to_string()],
        });
        let b = Some(TenantComposio {
            backend_url: "https://api.tinyhumans.ai".to_string(),
            auth_token: "token-b".to_string(),
            toolkits: vec!["gmail".to_string()],
        });
        assert_ne!(
            TenantComposio::fingerprint(&a),
            TenantComposio::fingerprint(&b),
            "a rotated token must move the fingerprint"
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
        TenantComposio {
            backend_url: url.to_string(),
            auth_token: "tenant-token".to_string(),
            toolkits,
        }
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
        TenantComposio {
            backend_url: url.to_string(),
            auth_token: token.to_string(),
            toolkits: Vec::new(),
        }
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
