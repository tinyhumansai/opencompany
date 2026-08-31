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
//! Three sources, in strict precedence:
//!
//! 1. **The company's own Composio token**, stored in its [`SecretStore`] under
//!    [`TOKEN_KEY`] by the console. A company that brings its own Composio
//!    identity keeps it, always. This is the self-hosting escape hatch, not a
//!    deployment mode.
//! 2. **The company's own TinyHumans credential** —
//!    [`company_key`](crate::company::company_key), the one key its admin set on
//!    this tenant. The backend derives the Composio entity from whatever bearer
//!    it is handed, and a TinyHumans key is a bearer it recognises, so no
//!    separate Composio token and no per-tenant provider app is needed to
//!    connect a provider (issue #586).
//! 3. **This instance's platform identity** — the
//!    [`TinyhumansTokenSource`](crate::company::credentials::TinyhumansTokenSource)
//!    the runtime authenticates with everywhere else. On the hosted platform that
//!    is a projected, audience-bound token the cluster rotates in place, so it is
//!    read **per call**, never captured when the roster is built.
//!
//! Tiers 2 and 3 are not resolved here: they are
//! [`company_key::resolve`](crate::company::company_key::resolve), the one seam
//! a brokered surface resolves a company identity through. That is what makes
//! rotating the company key reach every surface wired to it rather than
//! whichever remembered to re-read — Composio today, with inference and
//! embeddings still on the environment until #585.
//!
//! With none of the three, resolution yields `None` and no tools are wired (fail
//! closed) — an absent credential must mean "no tools", never a borrowed
//! identity. Two companies pasting the *same* token would share one entity; that
//! cannot be prevented client-side and is documented as a deployment caveat.
//!
//! ## One connection, every agent
//!
//! Nothing here is scoped to the member who connected a provider. The credential
//! is resolved from the *company's* store, every agent in the company resolves
//! the same one, and the backend derives one entity from it — so a provider
//! connected once is usable by every agent in the company, which is the
//! behaviour issue #586 asks for.
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
//! The token is write-only. Whatever value a call resolves is fed to
//! [`redact`](crate::harness::mcp_probe::redact) (successes) or
//! [`scrub`](crate::harness::mcp_probe::scrub) (errors) as a known secret, so it
//! cannot survive into **any** `ToolResult`; it is absent from every tracing
//! line and from the [`Debug`] impl. Only non-secret status (backend URL,
//! toolkit allowlist) is ever surfaced.
//!
//! ## Result sizing (issue #410)
//!
//! Successes go through `redact` + a **body** budget, never through `scrub`.
//! `scrub` caps at 300 bytes — correct for the one-line MCP failure sentence it
//! was built for, and the reason every Composio result used to arrive as a
//! silent fragment. See `scrubbed_ok` below and
//! [`composio_catalog`](crate::harness::composio_catalog).

use std::sync::Arc;

use crate::company::credentials::{Credential, TinyhumansTokenSource};
use crate::ports::SecretStore;
use crate::ports::types::CompanyId;

// The credential key + backend routing live in the always-compiled
// `company::composio` module (so the console read/write plane can manage the
// token in the default build); re-exported here for the harness call sites.
pub use crate::company::composio::{
    COMPOSIO_BACKEND_URL_ENV, TINYHUMANS_API_URL_ENV, TOKEN_KEY, backend_url_or_default,
    resolve_credential,
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
    /// Which connected account this company means, per toolkit (issue #820).
    ///
    /// Read from the company's own store by [`Self::resolve`], never from agent
    /// input: the id decides which Gmail an agent sends as, so it must be a
    /// company decision the same way the credential is.
    ///
    /// Empty — the ordinary case — means the company has expressed no intent and
    /// `composio_execute` sends no connection id, leaving the account to
    /// Composio's own resolution exactly as before.
    defaults: crate::company::composio::ComposioDefaults,
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
            defaults: Default::default(),
        }
    }

    /// The same config with this company's per-toolkit connection pins attached
    /// (issue #820).
    ///
    /// A builder rather than a fourth parameter on [`Self::new`]: every existing
    /// call site means "no pins", and the honest way to say that is to not say
    /// it.
    pub fn with_defaults(mut self, defaults: crate::company::composio::ComposioDefaults) -> Self {
        self.defaults = defaults;
        self
    }

    /// The connection id this company pinned for `toolkit`, if any.
    ///
    /// `toolkit` is matched as [`slug_toolkit`] produces it — lowercased — which
    /// is what [`crate::company::composio::set_default`] normalizes to on the way
    /// in.
    pub fn default_connection(&self, toolkit: &str) -> Option<&str> {
        self.defaults.get(toolkit).map(String::as_str)
    }

    /// Resolve a per-tenant Composio config, or `None` (fail closed) when no
    /// credential can be obtained at all.
    ///
    /// Precedence: the company's **own Composio** token under [`TOKEN_KEY`] wins
    /// — a company that pasted one keeps it even on the hosted platform. Failing
    /// that, the shared brokered-credential seam
    /// [`company_key::resolve`](crate::company::company_key::resolve) answers:
    /// the company's own TinyHumans key, else this instance's platform identity.
    /// A company that pasted nothing borrows no one else's identity — it presents
    /// its own key if it has one, otherwise the identity of the instance it runs
    /// in, which the backend resolves to that instance's owner.
    ///
    /// The URL resolves from [`COMPOSIO_BACKEND_URL_ENV`], then the tenant API
    /// base [`TINYHUMANS_API_URL_ENV`], then [`DEFAULT_BACKEND_URL`] — see
    /// [`backend_url_or_default`]. `toolkits` is the manifest allowlist, threaded
    /// through unchanged.
    ///
    /// A secret-store read error yields `None` — **fail closed, no tools this
    /// cycle** — rather than falling through to the instance identity. This is
    /// the roster path, so it must not bubble and brick a build; but an *unknown*
    /// credential must no more mean a borrowed identity than an absent one does.
    /// A company whose store hiccups loses its Composio tools for a cycle and
    /// gets them back on the next; it never quietly acts as somebody else. See
    /// [`company_key::resolve`](crate::company::company_key::resolve).
    pub async fn resolve(
        company: &CompanyId,
        secrets: &dyn SecretStore,
        toolkits: Vec<String>,
        backend_url_env: Option<String>,
        api_url_env: Option<String>,
        token_source: Option<Arc<TinyhumansTokenSource>>,
    ) -> Option<Self> {
        let credential = match crate::company::composio::resolve_credential(
            company,
            secrets,
            token_source,
        )
        .await
        {
            Ok(credential) => credential,
            Err(err) => {
                tracing::warn!(
                    company = %company,
                    error = %err,
                    "[composio] could not read this company's credential; withholding tools \
                     for this cycle rather than presenting another identity"
                );
                return None;
            }
        };
        match credential {
            Credential::None => None,
            credential => {
                // Which account the company means, per toolkit (issue #820).
                // Read here rather than per call so it lands in the fingerprint
                // below: changing the pin then rebuilds the roster on the next
                // turn, the same way a rotated token does, and no tool holds a
                // stale answer. A store hiccup on *this* read means "no
                // preference" — degrading to Composio's own resolution is the
                // behaviour that existed before the pin did, so it cannot
                // reroute anything.
                let defaults = crate::company::composio::load_defaults(company, secrets)
                    .await
                    .unwrap_or_default();
                Some(
                    Self::new(
                        backend_url_or_default(backend_url_env, api_url_env),
                        credential,
                        toolkits,
                    )
                    .with_defaults(defaults),
                )
            }
        }
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
    /// rebuild the whole roster on that schedule. A pasted per-company token —
    /// and the company's own TinyHumans key — does contribute its value, since
    /// an admin changing either is a real identity change and must reach the
    /// agents on the next cycle.
    ///
    /// **Internal only — never log, serialize, or journal this.** Because it is
    /// value-derived over a live credential, anyone who can read it can confirm
    /// a guessed key against it: cheap to check, expensive to discover has been
    /// leaking. It is compared for equality inside [`HarnessPool`] and goes
    /// nowhere else. If a rebuild needs explaining, say that the identity
    /// changed — not what it hashes to.
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
                // The pins are part of what the tools do, so a console change
                // to one has to reach the agents the same cycle a token change
                // does (issue #820). Safe to hash by value: a connection id is
                // not a credential.
                c.defaults.hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

/// Whether a toolkit is admitted by an allowlist. An **empty** allowlist defers
/// to the backend's server-enforced allowlist (open mode) and admits every
/// toolkit; a non-empty allowlist admits only its members (case-insensitive).
///
/// Every call site is inside the `composio`-gated [`live`] module, so this is
/// genuinely dead in an `openhuman`-without-`composio` build and is gated to
/// match. A blanket `#[allow(dead_code)]` would say the same thing to the
/// compiler while also hiding the day a real call site disappears.
#[cfg(feature = "composio")]
fn toolkit_allowed(allowlist: &[String], toolkit: &str) -> bool {
    allowlist.is_empty()
        || allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(toolkit))
}

/// The toolkit a Composio action slug belongs to: the segment before the first
/// `_`, lowercased (`GMAIL_SEND_EMAIL` → `gmail`). Empty slug → empty string.
///
/// Gated for the same reason as [`toolkit_allowed`]: both call sites live in
/// the `composio`-gated [`live`] module.
#[cfg(feature = "composio")]
fn slug_toolkit(slug: &str) -> String {
    slug.split('_').next().unwrap_or("").to_ascii_lowercase()
}

/// One connected Composio account, projected for the console (issue #404).
///
/// Composio models a connection as an **account**, not as a boolean: a company
/// can hold two Gmail connections, and telling them apart is the entire point of
/// a detail view. [`list_connection_states`] deliberately folds this down to one
/// `(toolkit, connected)` pair per toolkit for the tile grid and the
/// reconciliation probe, both of which only ever ask "is this provider wired".
/// Everything that needs to *manage* a connection reads these rows instead.
///
/// **Non-secret projection.** Composio returns no token material on this route,
/// and nothing here is derived from the tenant bearer. The [`id`](Self::id) is a
/// Composio-side handle, not a credential: it is the argument
/// [`delete_connection`] takes, and it is useless without the bearer that scopes
/// it to this company.
///
/// Always compiled, so the console DTO that mirrors it (`ops::composio`) can be
/// defined in a build without the `composio` feature — only the functions that
/// produce these rows are gated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposioConnectionRow {
    /// Composio's connection id — what [`delete_connection`] revokes.
    pub id: String,
    /// Toolkit slug, normalized (`gmail`, `googlecalendar`).
    pub toolkit: String,
    /// Composio's raw status string (`ACTIVE`, `INITIATED`, `EXPIRED`, …).
    ///
    /// Forwarded verbatim rather than reduced to [`connected`](Self::connected),
    /// because "connected: false" reads as *not set up* while an expired
    /// connection was set up and needs re-authorizing — a different sentence and
    /// a different button. The console maps the vocabulary; the host does not
    /// pretend to know every value Composio may add.
    pub status: String,
    /// Whether this connection is usable — `ACTIVE` or `CONNECTED`,
    /// case-insensitively, matching the vendored client's own `is_active`.
    pub connected: bool,
    /// When Composio recorded the connection, ISO-8601, when it says.
    pub created_at: Option<String>,
    /// The account label this connection acts as, when the provider published
    /// one: the account email, else a workspace/team name, else a handle.
    ///
    /// Derived here rather than in the console so the precedence is stated once
    /// and tested once — it mirrors OpenHuman's `deriveConnectionLabel`, which
    /// is the experience this issue ports. `None` is honest: plenty of toolkits
    /// publish no identity at all, and inventing one from the slug would render
    /// as a fact the operator cannot check.
    pub account: Option<String>,
}

/// Why a disconnect did not happen.
///
/// Two variants because they are two different sentences to an operator, and —
/// caught by running the route rather than by a test — two different HTTP
/// statuses. Collapsing both into one error type reported a refused id as
/// `502 Bad Gateway`: "the provider is down", about a call that was never made,
/// for an id the guard rejected locally. The caller cannot re-derive the
/// distinction from an error string, so the type carries it.
#[derive(Debug)]
pub enum DisconnectError {
    /// The id names nothing this company can see, so there is nothing to
    /// revoke. A client mistake, not an outage.
    NotFound(String),
    /// The call reached Composio, and Composio failed or declined it. Already
    /// scrubbed of the tenant bearer.
    Upstream(anyhow::Error),
}

impl std::fmt::Display for DisconnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) => f.write_str(message),
            Self::Upstream(err) => write!(f, "{err}"),
        }
    }
}

#[cfg(feature = "composio")]
pub use live::{
    ComposioMetering, authorize_connect_url, composio_tools, delete_connection,
    list_catalog_toolkits, list_connection_states, list_connections_detailed,
    set_default_connection,
};

#[cfg(feature = "composio")]
mod live {
    use super::*;

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use crate::company::composio::CatalogEntry;
    use crate::harness::composio_catalog as catalog;
    use crate::harness::mcp_probe::{redact, scrub};
    use crate::metering::record_oauth_call;
    use crate::ports::UsageMeter;
    use crate::ports::now_millis;

    use oh::integrations::IntegrationClient;
    use oh::integrations::composio::ComposioClient;
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

    /// Run a Composio action **as a named connected account** (issue #820).
    ///
    /// The vendored [`ComposioClient::execute_tool`] builds its body as
    /// `{tool, arguments}` and has no parameter for a connected account, so a
    /// company that holds two Gmail accounts has no way to say which one an
    /// agent sends from — the account is resolved by Composio for the entity,
    /// outside this codebase entirely. The platform backend's
    /// `POST /agent-integrations/composio/execute` *does* accept a
    /// `connectionId` and forwards it to Composio as `connectedAccountId`
    /// (`composioExecuteToolController`), so the only missing link was this
    /// body field.
    ///
    /// This is deliberately a **thin shim, not a fork**: every step below is the
    /// vendored client's own public helper, called in the vendored client's own
    /// order, so the two paths cannot drift on argument normalization, egress
    /// disclosure or provider-error rendering. It is reached **only** when the
    /// company has pinned an account; an unpinned call still goes through
    /// `execute_tool` verbatim, which is why the ordinary single-account
    /// company's behaviour is untouched by this change.
    ///
    /// The one behaviour it does not reproduce is the client's private
    /// single-shot post-OAuth retry, so it is re-stated here against the same
    /// error string — see [`POST_OAUTH_AUTH_ERROR`]. Delete all of this the day
    /// the vendored client's execute body takes a connection id.
    async fn execute_pinned(
        client: &ComposioClient,
        tool: &str,
        arguments: Option<Value>,
        connection_id: &str,
    ) -> Result<oh::integrations::composio::types::ComposioExecuteResponse> {
        use oh::security::egress::{EgressDescriptor, emit_external_transfer, enforce_egress};

        // Egress spine: disclose (and, under LocalOnly, refuse) the transfer
        // BEFORE the round-trip, exactly as `execute_tool` does. A pinned call
        // ships the same arguments to the same third party; it must not be a way
        // around the gate.
        let egress = EgressDescriptor::composio(tool);
        enforce_egress(&egress)?;
        emit_external_transfer(egress);

        let arguments =
            oh::integrations::composio::execute_prepare::prepare_execute_arguments(tool, arguments)
                .map_err(anyhow::Error::msg)?;
        let body = json!({
            "tool": tool,
            "arguments": arguments,
            "connectionId": connection_id,
        });
        // The connection id is not a credential (it is the same id the console
        // renders and `delete_connection` takes), so it may be traced — the
        // arguments still may not.
        tracing::debug!(tool = %tool, connection_id = %connection_id, "[composio] execute (pinned account)");

        let post = async |body: &Value| {
            client
                .inner()
                .post::<oh::integrations::composio::types::ComposioExecuteResponse>(
                    "/agent-integrations/composio/execute",
                    body,
                )
                .await
        };

        let mut resp = post(&body).await?;
        if is_post_oauth_auth_error(&resp) {
            tracing::debug!(
                tool = %tool,
                "[composio] pinned execute hit the post-OAuth readiness gap; retrying once"
            );
            tokio::time::sleep(POST_OAUTH_RETRY_DELAY).await;
            resp = post(&body).await?;
        }
        if !resp.successful
            && let Some(ref err) = resp.error
        {
            resp.error =
                Some(oh::integrations::composio::error_mapping::format_provider_error(tool, err));
        }
        Ok(resp)
    }

    /// Composio's gateway string for the window between a connection reporting
    /// `ACTIVE` and its token being usable for actions. Matched
    /// case-insensitively as a substring, mirroring the vendored client.
    const POST_OAUTH_AUTH_ERROR: &str = "connection error, try to authenticate";

    /// How long to wait before the single post-OAuth retry — the vendored
    /// client's own delay.
    const POST_OAUTH_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

    /// Whether a response is the post-OAuth readiness gap rather than a real
    /// refusal. Only the payload-level `successful:false` shape is eligible;
    /// transport errors have already propagated by this point.
    fn is_post_oauth_auth_error(
        resp: &oh::integrations::composio::types::ComposioExecuteResponse,
    ) -> bool {
        !resp.successful
            && resp
                .error
                .as_deref()
                .is_some_and(|err| err.to_ascii_lowercase().contains(POST_OAUTH_AUTH_ERROR))
    }

    /// Serialize a successful response to JSON, redact the tenant token out of
    /// it, and bound it to a *body* budget before it reaches the agent.
    /// Text-only output — the structured value is dropped so a credential the
    /// backend might reflect can never ride out in a JSON field.
    ///
    /// # Why not `scrub` (issue #410)
    ///
    /// This used to call [`scrub`], whose third pass caps its output at
    /// [`SCRUB_MAX_BYTES`](crate::harness::mcp_probe::SCRUB_MAX_BYTES) — 300
    /// bytes, the right size for the one-line MCP failure sentence it was built
    /// for and a catastrophe for a tool body. Every Composio result was cut to
    /// 300 bytes and terminated with a bare `…`: an action listing became the
    /// first action and half of its schema, and `composio_execute` returned 300
    /// bytes of whatever the provider actually said. Nothing in the result said
    /// it was a fragment, so the agent had no reason to ask differently and
    /// reissued the identical call until the repetition guard stopped the run.
    ///
    /// [`redact`] keeps the security half — the token replacement and the URL
    /// query strip — verbatim and unconditional; only the length decision moves
    /// here, where it can be sized for a body and describe its own cut.
    fn scrubbed_ok(value: Value, secrets: &[String], what: &str) -> ToolResult {
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        ToolResult::success(catalog::bound_body(redact(&text, secrets), what))
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

    /// Every connected account this company holds, one row per **connection**
    /// rather than per toolkit (issue #404). Backs the console's provider detail
    /// view and the disconnect it offers.
    ///
    /// Filtered to the tenant allowlist exactly as [`list_connection_states`] is
    /// — the two share this call, so a connection outside the company's grant is
    /// invisible to both, and cannot be reached by guessing its id either (see
    /// [`delete_connection`]). Sorted by `(toolkit, id)` for a stable render
    /// order. Any upstream error is scrubbed of the tenant bearer before it
    /// bubbles.
    pub async fn list_connections_detailed(
        config: &TenantComposio,
    ) -> Result<Vec<ComposioConnectionRow>> {
        tracing::debug!(allowlist = ?config.toolkits, "[composio] ops list_connections_detailed");
        let (client, secrets) = live_call(config).await?;
        let resp = match client.list_connections().await {
            Ok(resp) => resp,
            Err(err) => return Err(anyhow::anyhow!(scrub(&format!("{err}"), &secrets))),
        };
        let mut rows: Vec<ComposioConnectionRow> = resp
            .connections
            .into_iter()
            .filter_map(|conn| {
                let toolkit = conn.normalized_toolkit();
                if !toolkit_allowed(&config.toolkits, &toolkit) {
                    return None;
                }
                let connected = conn.is_active();
                Some(ComposioConnectionRow {
                    id: conn.id,
                    toolkit,
                    status: conn.status,
                    connected,
                    created_at: conn.created_at,
                    // Same precedence as OpenHuman's `deriveConnectionLabel`:
                    // email, then workspace, then handle. A field present but
                    // blank is treated as absent — a whitespace label would
                    // render as an empty parenthetical, which reads as a bug.
                    account: [conn.account_email, conn.workspace, conn.username]
                        .into_iter()
                        .flatten()
                        .map(|v| v.trim().to_string())
                        .find(|v| !v.is_empty()),
                })
            })
            .collect();
        rows.sort_by(|a, b| a.toolkit.cmp(&b.toolkit).then_with(|| a.id.cmp(&b.id)));
        Ok(rows)
    }

    /// The per-toolkit connected state the console renders as provider tiles: one
    /// `(toolkit, connected)` pair per toolkit that has at least one connection,
    /// with `connected == true` when **any** connection for that toolkit is
    /// active. Backs the console's `GET …/composio/connections` route and the
    /// reconciliation probe in `ops::connections_read`.
    ///
    /// A projection over [`list_connections_detailed`] rather than a second call
    /// shape: one network round-trip, one allowlist filter, one scrub. The fold
    /// is what the tile grid wants and all the probe can use — both ask only
    /// "is this provider wired" — but it is lossy, so anything that manages a
    /// connection reads the rows instead.
    pub async fn list_connection_states(config: &TenantComposio) -> Result<Vec<(String, bool)>> {
        let rows = list_connections_detailed(config).await?;
        let mut states: std::collections::BTreeMap<String, bool> =
            std::collections::BTreeMap::new();
        for row in rows {
            states
                .entry(row.toolkit)
                .and_modify(|c| *c = *c || row.connected)
                .or_insert(row.connected);
        }
        Ok(states.into_iter().collect())
    }

    /// Revoke one connected account by its Composio connection id (issue #404).
    /// Backs the console's `DELETE …/composio/connections/{id}`.
    ///
    /// **The id is checked against this tenant's own filtered list first**, and
    /// an unknown one fails before any delete is attempted. Two reasons, and the
    /// second is the load-bearing one:
    ///
    /// * The backend scopes a delete to the caller's bearer, so another
    ///   company's connection was never reachable — but a connection belonging
    ///   to *this* company under a toolkit its manifest does **not** allow is
    ///   reachable by that bearer, and is deliberately invisible to every read
    ///   here. Letting an id delete what no read will show would make the
    ///   allowlist a display filter rather than a boundary.
    /// * It turns "already disconnected" into a clear answer instead of whatever
    ///   the upstream returns for a stale id.
    ///
    /// Returns `Ok(())` on a completed revoke. A refusal (`deleted: false`) is an
    /// error rather than a silent success: the console's next line tells the
    /// operator the account is gone, and it must not say so on the strength of a
    /// call the backend declined.
    pub async fn delete_connection(
        config: &TenantComposio,
        connection_id: &str,
    ) -> std::result::Result<(), DisconnectError> {
        let connection_id = connection_id.trim();
        if connection_id.is_empty() {
            return Err(DisconnectError::NotFound(
                "a connection id is required".to_string(),
            ));
        }
        let known = list_connections_detailed(config)
            .await
            .map_err(DisconnectError::Upstream)?;
        if !known.iter().any(|row| row.id == connection_id) {
            return Err(DisconnectError::NotFound(
                "no such connection for this company".to_string(),
            ));
        }
        tracing::debug!(connection_id = %connection_id, "[composio] ops delete_connection");
        let (client, secrets) = live_call(config).await.map_err(DisconnectError::Upstream)?;
        match client.delete_connection(connection_id).await {
            Ok(resp) if resp.deleted => Ok(()),
            Ok(_) => Err(DisconnectError::Upstream(anyhow::anyhow!(
                "Composio declined to delete the connection"
            ))),
            Err(err) => Err(DisconnectError::Upstream(anyhow::anyhow!(scrub(
                &format!("{err}"),
                &secrets
            )))),
        }
    }

    /// Pin the toolkit of `connection_id` to that account, so every
    /// `composio_execute` for it acts as that account (issue #820). Backs the
    /// console's `PUT …/composio/connections/{id}/default`.
    ///
    /// **The id is checked against this tenant's own filtered list first**, for
    /// the same two reasons [`delete_connection`] checks it, and one more that
    /// only applies here: an unchecked id would be stored, and a stored id that
    /// names nothing is not an error the operator sees at write time — it is a
    /// toolkit that stops working at the next agent turn, for a reason nothing
    /// on screen explains. Failing the write is the only place the mistake is
    /// still legible.
    ///
    /// Returns the toolkit that was pinned, which is the one the console needs
    /// to re-render and never has to guess at.
    pub async fn set_default_connection(
        config: &TenantComposio,
        company: &CompanyId,
        secrets: &dyn SecretStore,
        connection_id: &str,
    ) -> std::result::Result<String, DisconnectError> {
        let connection_id = connection_id.trim();
        if connection_id.is_empty() {
            return Err(DisconnectError::NotFound(
                "a connection id is required".to_string(),
            ));
        }
        let known = list_connections_detailed(config)
            .await
            .map_err(DisconnectError::Upstream)?;
        let Some(row) = known.iter().find(|row| row.id == connection_id) else {
            return Err(DisconnectError::NotFound(
                "no such connection for this company".to_string(),
            ));
        };
        // An account that is not usable is refused rather than stored: pinning
        // an EXPIRED connection would route every send for the toolkit to an
        // account that cannot send, which is worse than the unpinned behaviour
        // it replaces. Re-authorize it first, then pin it.
        if !row.connected {
            return Err(DisconnectError::NotFound(format!(
                "that account is `{}`, not connected — re-authorize it before making it the default",
                row.status
            )));
        }
        let toolkit = row.toolkit.clone();
        tracing::debug!(connection_id = %connection_id, toolkit = %toolkit, "[composio] ops set_default_connection");
        crate::company::composio::set_default(company, secrets, &toolkit, connection_id)
            .await
            .map_err(|err| DisconnectError::Upstream(anyhow::anyhow!("{err}")))?;
        Ok(toolkit)
    }

    /// The backend's live Composio toolkit catalog — every slug it will let
    /// this tenant connect. Backs the console's open-mode provider list
    /// (issue #397).
    ///
    /// This is the same `GET /agent-integrations/composio/toolkits` call the
    /// `composio_list_toolkits` agent tool makes and the same one OpenHuman's
    /// Skills grid drives off, so the console offers what the backend actually
    /// permits instead of a list maintained by hand here.
    ///
    /// Deliberately **not** filtered by the tenant allowlist, unlike
    /// [`list_connection_states`]: the only caller is the open-mode path, where
    /// the allowlist is empty by definition. A company with a non-empty
    /// allowlist is offered its own list verbatim and never reaches this
    /// function — the catalog must not be able to widen a manifest that
    /// deliberately narrowed.
    ///
    /// Entries are the catalog's **connectable** slugs — `enabled == true`,
    /// mirroring the vendored runtime's own `connectable_toolkit_slugs`, since
    /// advertising a provider the backend gate will refuse only invites a failed
    /// sign-in. Backends predating the dynamic catalog send no `catalog[]` at
    /// all; their plain slug allowlist is used instead. Slugs are trimmed,
    /// lowercased, de-duplicated and sorted for a stable render order. Any
    /// upstream error is scrubbed of the tenant bearer before it bubbles.
    ///
    /// ## Why this returns entries rather than slugs (issue #600)
    ///
    /// It used to return `Vec<String>`, and that one `.map(|e| e.slug)` was the
    /// whole of #600. The backend publishes `name`, `logo`, `description` and
    /// `categories` on every entry and states plainly that it assembles them so
    /// the frontend can read them straight from there — and this function threw
    /// five of the six fields away one layer before the console, which then had
    /// nothing to group by, nothing to brand with, and nothing to search but the
    /// slug. A hundred-and-twenty-three-item flat list was the honest rendering
    /// of what it was handed.
    ///
    /// Nothing about *admission* changed. The agent-side gate is
    /// [`toolkit_allowed`], which takes slugs and never consulted this function;
    /// the console's slug list is still derived from these entries. This widens
    /// what is *described*, not what is permitted.
    pub async fn list_catalog_toolkits(config: &TenantComposio) -> Result<Vec<CatalogEntry>> {
        tracing::debug!("[composio] ops list_catalog_toolkits");
        let (client, secrets) = live_call(config).await?;
        let resp = match client.list_toolkits().await {
            Ok(resp) => resp,
            Err(err) => return Err(anyhow::anyhow!(scrub(&format!("{err}"), &secrets))),
        };
        let normalize = |slug: &str| slug.trim().to_ascii_lowercase();
        // A `BTreeMap` keyed on the normalized slug keeps the de-duplication and
        // the stable sort the slug set gave us, while carrying the metadata that
        // is the entire point of the widening.
        //
        // `or_insert_with`, NOT `collect()` into the map: collecting keeps the
        // LAST value for a repeated key, and a duplicate catalog entry is
        // typically the degenerate one — the mock's `Gmail (dup)` carries no
        // description, and collecting would let it silently blank the real
        // Gmail's. First entry wins, which is also what the `BTreeSet<String>`
        // this replaced effectively did.
        let mut entries: std::collections::BTreeMap<String, CatalogEntry> =
            std::collections::BTreeMap::new();
        for entry in resp.catalog.iter().filter(|e| e.enabled.unwrap_or(false)) {
            let slug = normalize(&entry.slug);
            if slug.is_empty() {
                continue;
            }
            entries.entry(slug.clone()).or_insert_with(|| CatalogEntry {
                slug,
                name: entry.name.trim().to_string(),
                description: entry
                    .description
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string(),
                logo: entry
                    .logo
                    .as_deref()
                    .map(str::trim)
                    .filter(|logo| !logo.is_empty())
                    .map(str::to_string),
                categories: entry
                    .categories
                    .iter()
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
                    .collect(),
            });
        }
        if entries.is_empty() {
            // A backend predating the dynamic catalog. Slugs are all it has, so
            // slugs are all the console gets — rendered with local typography
            // rather than dropped.
            entries = resp
                .toolkits
                .iter()
                .map(|slug| normalize(slug))
                .filter(|slug| !slug.is_empty())
                .map(|slug| (slug.clone(), CatalogEntry::from_slug(slug)))
                .collect();
        }
        Ok(entries.into_values().collect())
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
            catalog::list_toolkits_description()
        }

        fn parameters_schema(&self) -> Value {
            catalog::list_toolkits_parameters_schema()
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::ReadOnly
        }

        async fn execute(&self, args: Value) -> Result<ToolResult> {
            let request = catalog::ToolkitListRequest::parse(&args);
            tracing::debug!(
                allowlist = ?self.toolkits,
                search = ?request.search,
                limit = request.limit,
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
                    // Issue #410: bounded, self-describing rendering rather than
                    // the whole catalogue as pretty JSON. Composio publishes
                    // several hundred toolkits, each with prose and a categories
                    // array, so this listing is the same silent-cut class as the
                    // action listing one level down.
                    let catalogued: Vec<catalog::CatalogToolkit> = resp
                        .catalog
                        .iter()
                        .map(|entry| catalog::CatalogToolkit {
                            slug: entry.slug.clone(),
                            name: entry.name.clone(),
                            description: entry.description.clone().unwrap_or_default(),
                            connected: entry.enabled,
                        })
                        .collect();
                    // Backends predating the dynamic catalogue send only the
                    // slug allowlist; render those slugs rather than nothing.
                    let toolkits: Vec<catalog::CatalogToolkit> = if catalogued.is_empty() {
                        resp.toolkits
                            .iter()
                            .map(|slug| catalog::CatalogToolkit {
                                slug: slug.clone(),
                                name: String::new(),
                                description: String::new(),
                                connected: None,
                            })
                            .collect()
                    } else {
                        catalogued
                    };
                    let rendered = catalog::render_toolkits(&toolkits, &request);
                    Ok(ToolResult::success(redact(&rendered, &secrets)))
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
                        "connections list",
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
            catalog::list_tools_description()
        }

        fn parameters_schema(&self) -> Value {
            catalog::list_tools_parameters_schema()
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
            // The narrowing + rendering request (issue #410). Toolkit resolution
            // above is a security decision (allowlist intersection) and stays
            // here; `search` / `detail` / `limit` are presentation and live in
            // the pure catalogue module.
            let request = catalog::ListRequest::parse(&args, effective.clone());
            tracing::debug!(
                effective = ?effective,
                allowlist = ?self.toolkits,
                search = ?request.search,
                detail = ?request.detail,
                limit = request.limit,
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
                    // Issue #410: render through the bounded, self-describing
                    // catalogue view rather than dumping the whole response as
                    // pretty JSON. A hundred-action toolkit serialized whole is
                    // hundreds of kilobytes; the harness's shared tool-result
                    // budget then cut it on a byte boundary, leaving the agent a
                    // fragment with nothing in it saying so.
                    let actions: Vec<catalog::CatalogAction> = resp
                        .tools
                        .iter()
                        .map(|schema| catalog::CatalogAction {
                            toolkit: slug_toolkit(&schema.function.name),
                            slug: schema.function.name.clone(),
                            description: schema.function.description.clone().unwrap_or_default(),
                            parameters: schema.function.parameters.clone(),
                        })
                        .collect();
                    let rendered = catalog::render(&actions, &request);
                    Ok(ToolResult::success(redact(&rendered, &secrets)))
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
                    "authorization response",
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
            "Run a Composio action by its slug (e.g. `GMAIL_SEND_EMAIL`) with a JSON `arguments` object. Discover the slug first with `composio_list_tools({\"search\": \"<words>\"})`, then read its parameters with `composio_list_tools({\"search\": \"<SLUG>\", \"detail\": \"schemas\"})`. Never guess a slug that was not listed."
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
            // Which account this company means for the toolkit, if it has said
            // (issue #820). Resolved from the company's own config — never from
            // `args` — because "send from billing@, not ops@" is a company
            // decision, and an agent that could name a connection could name one
            // the operator deliberately did not choose.
            let pinned = self.config.default_connection(&toolkit).map(str::to_string);
            // tracing carries the slug/toolkit only — NEVER arguments or bodies.
            tracing::debug!(tool = %tool, toolkit = %toolkit, pinned = ?pinned, "[composio] execute");
            let (client, secrets) = match live_call(&self.config).await {
                Ok(live) => live,
                Err(err) => {
                    return Ok(ToolResult::error(format!("composio_execute failed: {err}")));
                }
            };
            // No pin — the ordinary case — is the untouched path: the same call
            // this tool has always made, with no connection id, resolved by
            // Composio for the entity.
            let call = match pinned.as_deref() {
                None => client.execute_tool(&tool, arguments).await,
                Some(connection_id) => {
                    execute_pinned(&client, &tool, arguments, connection_id).await
                }
            };
            match call {
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
                        &format!("`{tool}` output"),
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

        // ── which account the call acts as (issue #820) ──────────────────
        //
        // These assert on the **wire body**, not on a return value, because the
        // whole of #820 is a field that was missing from it: a test that only
        // checked the result would have passed before the change and after it.

        /// Every execute body a stub backend saw.
        type Bodies = Arc<Mutex<Vec<Value>>>;

        /// A backend that records each `POST …/composio/execute` body and
        /// answers with a successful, empty result.
        async fn spawn_execute_recorder() -> (String, Bodies) {
            use axum::Router;
            use axum::routing::post;

            let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&bodies);
            let app = Router::new().route(
                "/agent-integrations/composio/execute",
                post(async move |axum::Json(body): axum::Json<Value>| {
                    seen.lock().unwrap().push(body);
                    axum::Json(json!({
                        "success": true,
                        "data": { "data": {"ok": true}, "successful": true, "error": null }
                    }))
                }),
            );
            let listener =
                tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                    .await
                    .unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            (format!("http://{addr}"), bodies)
        }

        /// An execute tool over `url`, admitting gmail + slack, carrying
        /// `defaults` as the company's pins.
        fn tool_over(
            url: &str,
            defaults: &[(&str, &str)],
        ) -> (ComposioExecuteTool, Arc<RecordingMeter>) {
            let meter = Arc::new(RecordingMeter::default());
            let toolkits = vec!["gmail".to_string(), "slack".to_string()];
            let config = TenantComposio::new(
                url.to_string(),
                Credential::from_value("token"),
                toolkits.clone(),
            )
            .with_defaults(
                defaults
                    .iter()
                    .map(|(t, id)| (t.to_string(), id.to_string()))
                    .collect(),
            );
            (
                ComposioExecuteTool {
                    config: Arc::new(config),
                    toolkits: Arc::new(toolkits),
                    metering: ComposioMetering {
                        company: CompanyId::new("acme"),
                        agent: "ceo".to_string(),
                        meter: Some(Arc::clone(&meter) as Arc<dyn UsageMeter>),
                    },
                },
                meter,
            )
        }

        /// The ordinary company — one account per toolkit, nothing pinned —
        /// must send exactly the body it sent before #820, with no connection
        /// id at all. Sending one would change which account Composio resolves
        /// for every existing company.
        #[tokio::test]
        async fn an_unpinned_call_names_no_connection() {
            let (url, bodies) = spawn_execute_recorder().await;
            let (tool, meter) = tool_over(&url, &[]);

            let result = tool
                .execute(json!({"tool": "GMAIL_SEND_EMAIL", "arguments": {"to": "a@b.test"}}))
                .await
                .expect("execute returns a result");
            assert!(!result.is_error, "the call should succeed: {result:?}");

            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1);
            assert_eq!(bodies[0]["tool"], json!("GMAIL_SEND_EMAIL"));
            assert!(
                bodies[0].get("connectionId").is_none(),
                "an unpinned call must carry no connection id: {}",
                bodies[0]
            );
            assert_eq!(meter.samples.lock().unwrap().len(), 1, "still metered");
        }

        /// The point of the issue: a company that said "send as billing@" has
        /// that carried to the backend, which forwards it to Composio as the
        /// connected account.
        #[tokio::test]
        async fn a_pinned_toolkit_sends_its_connection_id() {
            let (url, bodies) = spawn_execute_recorder().await;
            let (tool, meter) = tool_over(&url, &[("gmail", "ca_billing")]);

            let result = tool
                .execute(json!({"tool": "GMAIL_SEND_EMAIL", "arguments": {"to": "a@b.test"}}))
                .await
                .expect("execute returns a result");
            assert!(!result.is_error, "the call should succeed: {result:?}");

            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1);
            assert_eq!(bodies[0]["connectionId"], json!("ca_billing"));
            assert_eq!(
                bodies[0]["arguments"]["to"],
                json!("a@b.test"),
                "the pinned path still normalizes and forwards the arguments"
            );
            assert_eq!(
                meter.samples.lock().unwrap().len(),
                1,
                "a pinned call is metered like any other"
            );
        }

        /// A pin is per toolkit, so one on gmail must not reach a slack call —
        /// the toolkit is derived from the slug, the same prefix the allowlist
        /// is enforced on.
        #[tokio::test]
        async fn a_pin_does_not_leak_across_toolkits() {
            let (url, bodies) = spawn_execute_recorder().await;
            let (tool, _) = tool_over(&url, &[("gmail", "ca_billing")]);

            tool.execute(json!({"tool": "SLACK_POST_MESSAGE", "arguments": {}}))
                .await
                .expect("execute returns a result");

            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1);
            assert!(
                bodies[0].get("connectionId").is_none(),
                "slack was never pinned: {}",
                bodies[0]
            );
        }

        /// The allowlist is still enforced on the slug prefix before anything
        /// is sent — a pin is not a way past it.
        #[tokio::test]
        async fn a_pin_does_not_widen_the_allowlist() {
            let (url, bodies) = spawn_execute_recorder().await;
            let (mut tool, _) = tool_over(&url, &[("notion", "ca_notion")]);
            tool.toolkits = Arc::new(vec!["gmail".to_string()]);

            let result = tool
                .execute(json!({"tool": "NOTION_CREATE_PAGE"}))
                .await
                .expect("execute returns a result");
            assert!(result.is_error, "notion is outside the allowlist");
            assert!(
                bodies.lock().unwrap().is_empty(),
                "nothing should have been sent"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Used by the resolver tests below; the module body itself resolves its
    // credential through `company::composio::resolve_credential`.
    use crate::company::company_key;
    use crate::ports::types::SecretValue;

    // The three helper tests below follow their subjects behind the `composio`
    // feature: `toolkit_allowed` / `slug_toolkit` do not exist in an
    // `openhuman`-without-`composio` build.
    #[cfg(feature = "composio")]
    #[test]
    fn toolkit_allowed_empty_defers_to_backend() {
        // Empty allowlist = open mode: every toolkit admitted.
        assert!(toolkit_allowed(&[], "gmail"));
        assert!(toolkit_allowed(&[], "anything"));
    }

    #[cfg(feature = "composio")]
    #[test]
    fn toolkit_allowed_non_empty_narrows_case_insensitively() {
        let allow = vec!["gmail".to_string(), "github".to_string()];
        assert!(toolkit_allowed(&allow, "gmail"));
        assert!(toolkit_allowed(&allow, "GMAIL"));
        assert!(toolkit_allowed(&allow, "GitHub"));
        assert!(!toolkit_allowed(&allow, "slack"));
    }

    #[cfg(feature = "composio")]
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
        use crate::ports::types::CompanyId;
        use crate::store::FsSecretStore;

        let dir = tempfile::Builder::new()
            .prefix("oc-composio-res-")
            .tempdir()
            .expect("tempdir");
        let secrets = FsSecretStore::new(dir.path());
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
    }

    /// Issue #586: the company's own TinyHumans key sits between its pasted
    /// Composio token and the instance's identity, and it is enough on its own —
    /// a company with a key set connects providers with no Composio token and no
    /// per-tenant provider app.
    #[tokio::test]
    async fn the_company_key_credentials_composio_between_a_byo_token_and_the_instance() {
        use crate::company::credentials::CredentialSource;
        use crate::ports::SecretStore;
        use crate::ports::types::CompanyId;
        use crate::store::FsSecretStore;

        let dir = tempfile::Builder::new()
            .prefix("oc-composio-companykey-")
            .tempdir()
            .expect("tempdir");
        let secrets = FsSecretStore::new(dir.path());
        let company = CompanyId::new("acme");
        let source = || Arc::new(TinyhumansTokenSource::static_key("platform-identity"));

        company_key::store_key(&company, &secrets, "th_company_key")
            .await
            .unwrap();

        // With no instance identity at all, the company key alone credentials
        // Composio — the case this issue exists to fix, since a pod with no
        // projected token previously had to fall back to a pasted token.
        let resolved = TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, None)
            .await
            .expect("the company key resolves without any instance identity");
        assert_eq!(token_of(&resolved).await.as_deref(), Some("th_company_key"));
        assert_eq!(resolved.credential().source(), CredentialSource::Company);

        // And it outranks the instance's identity: the company acts as itself,
        // not as the pod it happens to run in.
        let resolved =
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, Some(source()))
                .await
                .expect("resolves");
        assert_eq!(token_of(&resolved).await.as_deref(), Some("th_company_key"));

        // A pasted Composio token still outranks it — the BYO hatch survives.
        secrets
            .set(&company, TOKEN_KEY, SecretValue("byo-composio".to_string()))
            .await
            .unwrap();
        let resolved =
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, Some(source()))
                .await
                .expect("resolves");
        assert_eq!(token_of(&resolved).await.as_deref(), Some("byo-composio"));
        assert_eq!(resolved.credential().source(), CredentialSource::Static);

        // Clearing the BYO token falls back to the company key, not to the
        // instance — clearing one tier must not silently re-borrow another.
        secrets
            .set(&company, TOKEN_KEY, SecretValue(String::new()))
            .await
            .unwrap();
        let resolved =
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, Some(source()))
                .await
                .expect("resolves");
        assert_eq!(token_of(&resolved).await.as_deref(), Some("th_company_key"));

        // Clearing the company key too falls all the way back to the instance.
        company_key::store_key(&company, &secrets, "")
            .await
            .unwrap();
        let resolved =
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, Some(source()))
                .await
                .expect("resolves");
        assert_eq!(
            token_of(&resolved).await.as_deref(),
            Some("platform-identity")
        );
    }

    /// The roster path's half of the store-error contract: it must **fail
    /// closed** — no tools this cycle — rather than fall through to the
    /// instance identity or bubble and brick the build.
    ///
    /// Fewer tools for a cycle is recoverable and visible. Silently presenting
    /// a different identity is neither: it would attribute whatever the agents
    /// did in that window to the wrong account.
    #[tokio::test]
    async fn an_unreadable_store_withholds_tools_rather_than_borrowing_an_identity() {
        use crate::ports::types::CompanyId;

        struct BrokenSecrets;

        #[async_trait::async_trait]
        impl SecretStore for BrokenSecrets {
            async fn get(
                &self,
                _c: &CompanyId,
                _key: &str,
            ) -> crate::Result<Option<crate::ports::types::SecretValue>> {
                Err(crate::error::OpenCompanyError::Store("boom".into()))
            }
            async fn set(
                &self,
                _c: &CompanyId,
                _key: &str,
                _v: crate::ports::types::SecretValue,
            ) -> crate::Result<()> {
                Err(crate::error::OpenCompanyError::Store("boom".into()))
            }
        }

        let company = CompanyId::new("acme");
        let resolved = TenantComposio::resolve(
            &company,
            &BrokenSecrets,
            Vec::new(),
            None,
            None,
            // An instance identity IS available — and must still not be used,
            // because we cannot tell whether this company has a key of its own.
            Some(Arc::new(TinyhumansTokenSource::static_key(
                "platform-identity",
            ))),
        )
        .await;
        assert!(
            resolved.is_none(),
            "an unreadable store must withhold the tools, not present the instance's identity"
        );
    }

    /// The rotation guarantee (issue #586 acceptance): rotating the company key
    /// moves the roster fingerprint, so agents cannot be left on the previous
    /// credential after a console rotation.
    #[tokio::test]
    async fn rotating_the_company_key_moves_the_composio_fingerprint() {
        use crate::ports::types::CompanyId;
        use crate::store::FsSecretStore;

        let dir = tempfile::Builder::new()
            .prefix("oc-composio-rotate-")
            .tempdir()
            .expect("tempdir");
        let secrets = FsSecretStore::new(dir.path());
        let company = CompanyId::new("acme");

        let resolve = async || {
            TenantComposio::resolve(&company, &secrets, Vec::new(), None, None, None).await
        };

        company_key::store_key(&company, &secrets, "key-a")
            .await
            .unwrap();
        let before = TenantComposio::fingerprint(&resolve().await);

        company_key::store_key(&company, &secrets, "key-b")
            .await
            .unwrap();
        let after = TenantComposio::fingerprint(&resolve().await);
        assert_ne!(
            before, after,
            "a rotated company key must rebuild the roster, or agents keep the old credential"
        );

        // And clearing it is a change too — the roster must drop the tools.
        company_key::store_key(&company, &secrets, "")
            .await
            .unwrap();
        assert_eq!(
            TenantComposio::fingerprint(&resolve().await),
            TenantComposio::fingerprint(&None),
            "a cleared credential resolves to nothing, so no tools are wired"
        );
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
        let dir = tempfile::Builder::new()
            .prefix("oc-composio-fp-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
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
            TinyhumansTokenSource::projected_file(dir.path().join("other")),
        )));
        assert_ne!(TenantComposio::fingerprint(&other), before);
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

    use crate::company::composio::CatalogEntry;

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
    ///
    /// The identity fields exercise each arm of the account-label precedence
    /// (issue #404): `c1` publishes an email, `c2` only a blank one plus a
    /// workspace, `c3` only a username, `c4` nothing at all.
    async fn connections_handler() -> axum::Json<Value> {
        axum::Json(json!({
            "success": true,
            "data": { "connections": [
                {
                    "id": "c1", "toolkit": "gmail", "status": "ACTIVE",
                    "createdAt": "2026-08-01T10:00:00Z",
                    "accountEmail": " ops@acme.test ",
                    "username": "ignored-when-an-email-is-present"
                },
                {
                    "id": "c2", "toolkit": "gmail", "status": "INITIATED",
                    "accountEmail": "   ",
                    "workspace": "Acme Workspace"
                },
                { "id": "c3", "toolkit": "slack", "status": "INITIATED", "username": "acme-bot" },
                { "id": "c4", "toolkit": "notion", "status": "ACTIVE" }
            ] }
        }))
    }

    /// Mock `GET /agent-integrations/composio/toolkits` — the dynamic catalog
    /// shape (backend #1012): a `toolkits` allowlist plus a `catalog[]` whose
    /// entries carry an `enabled` gate. `zendesk` is present but not connectable
    /// and must not be advertised; the casing and whitespace on `HubSpot` must
    /// normalise.
    ///
    /// Entries carry the display metadata (`logo`, `description`, `categories`)
    /// the backend actually publishes — issue #600 is that all of it was dropped
    /// on the way through, so a mock that omitted it could not have caught the
    /// bug.
    async fn toolkits_handler() -> axum::Json<Value> {
        axum::Json(json!({
            "success": true,
            "data": {
                "toolkits": ["gmail", "slack"],
                "catalog": [
                    {
                        "slug": " HubSpot ",
                        "name": "HubSpot",
                        "enabled": true,
                        "logo": " https://logos.composio.dev/api/hubspot ",
                        "description": "  CRM and marketing automation.  ",
                        "categories": ["crm", " marketing ", ""]
                    },
                    {
                        "slug": "gmail",
                        "name": "Gmail",
                        "enabled": true,
                        "description": "Send and read email.",
                        "categories": ["email"]
                    },
                    { "slug": "zendesk", "name": "Zendesk", "enabled": false },
                    { "slug": "gmail", "name": "Gmail (dup)", "enabled": true }
                ]
            }
        }))
    }

    /// Mock toolkits route for a backend predating the dynamic catalog: the
    /// plain slug allowlist and no `catalog[]` at all.
    async fn legacy_toolkits_handler() -> axum::Json<Value> {
        axum::Json(json!({
            "success": true,
            "data": { "toolkits": ["Notion", "gmail", ""] }
        }))
    }

    async fn spawn_backend() -> String {
        spawn_backend_with(get(toolkits_handler)).await
    }

    async fn spawn_backend_with(toolkits: axum::routing::MethodRouter) -> String {
        let app = Router::new()
            .route(
                "/agent-integrations/composio/authorize",
                post(authorize_handler),
            )
            .route(
                "/agent-integrations/composio/connections",
                get(connections_handler),
            )
            .route("/agent-integrations/composio/toolkits", toolkits);
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

    /// Issue #404: the detail view needs the account behind a connection, not
    /// just that one exists. Pins the whole projection — per-connection rows
    /// (two for gmail, where the fold gives one), the raw status, the account
    /// label precedence, and the `(toolkit, id)` order — against the same
    /// allowlist filter the fold applies.
    #[tokio::test]
    async fn list_connections_detailed_projects_each_account_with_its_identity() {
        let url = spawn_backend().await;
        let rows = list_connections_detailed(&config(&url, vec!["gmail".into(), "slack".into()]))
            .await
            .expect("list connections");

        // Compared as whole rows rather than as a tuple projection, so a field
        // added to `ComposioConnectionRow` later cannot slip past this
        // assertion unexamined.
        let expect = |id: &str,
                      toolkit: &str,
                      status: &str,
                      connected: bool,
                      created_at: Option<&str>,
                      account: Option<&str>| ComposioConnectionRow {
            id: id.to_string(),
            toolkit: toolkit.to_string(),
            status: status.to_string(),
            connected,
            created_at: created_at.map(str::to_string),
            account: account.map(str::to_string),
        };
        assert_eq!(
            rows,
            vec![
                // Email wins over the username the same row carries, and is
                // trimmed.
                expect(
                    "c1",
                    "gmail",
                    "ACTIVE",
                    true,
                    Some("2026-08-01T10:00:00Z"),
                    Some("ops@acme.test"),
                ),
                // A blank email is not an email: falls through to the workspace.
                // Kept as its own row rather than folded into c1 — this is the
                // "two Gmail accounts" case a disconnect has to tell apart.
                expect(
                    "c2",
                    "gmail",
                    "INITIATED",
                    false,
                    None,
                    Some("Acme Workspace"),
                ),
                // Username is the last resort.
                expect("c3", "slack", "INITIATED", false, None, Some("acme-bot")),
            ],
            "one row per connection, sorted by (toolkit, id); notion filtered out \
             by the allowlist exactly as the fold filters it"
        );
    }

    /// The fold the tile grid and the reconciliation probe read must keep
    /// meaning what it meant before #404 widened the call underneath it —
    /// `connected` is still "any account active", not "the first one".
    #[tokio::test]
    async fn the_per_toolkit_fold_still_summarises_the_detailed_rows() {
        let url = spawn_backend().await;
        let cfg = config(&url, vec!["gmail".into(), "slack".into()]);
        let rows = list_connections_detailed(&cfg).await.expect("rows");
        let states = list_connection_states(&cfg).await.expect("states");

        let folded: std::collections::BTreeMap<String, bool> =
            rows.into_iter().fold(Default::default(), |mut acc, r| {
                let e = acc.entry(r.toolkit).or_insert(false);
                *e = *e || r.connected;
                acc
            });
        assert_eq!(
            states,
            folded.into_iter().collect::<Vec<_>>(),
            "the states route is exactly the OR-fold of the detailed rows"
        );
    }

    /// Issue #404 + #403: an id this company's own reads will not show must not
    /// be deletable by naming it. The mock serves no DELETE route at all, so a
    /// request that got as far as dialling would fail loudly rather than pass —
    /// the refusal has to come from the guard, before the call.
    #[tokio::test]
    async fn disconnect_refuses_an_id_outside_this_companys_visible_connections() {
        let url = spawn_backend().await;
        // `c4` (notion) is a real, active connection upstream — but this
        // company's manifest does not grant notion, so no read here surfaces
        // it. That is the case the guard exists for: the bearer *could* delete
        // it, and the allowlist must be a boundary rather than a display filter.
        let err = delete_connection(&config(&url, vec!["gmail".into()]), "c4")
            .await
            .expect_err("a connection outside the grant is not deletable");
        // The *variant* is the assertion, not the message: it is what decides
        // the status code the console sees, and asserting only on the string
        // is what let a refusal ship as a `502`.
        assert!(
            matches!(err, DisconnectError::NotFound(_)),
            "a refused id must be NotFound, not an upstream failure: {err:?}"
        );

        // And an id that exists nowhere at all fails the same way.
        let err = delete_connection(&config(&url, vec!["gmail".into()]), "nope")
            .await
            .expect_err("an unknown id is not deletable");
        assert!(
            matches!(err, DisconnectError::NotFound(_)),
            "unexpected error: {err:?}"
        );
    }

    /// An empty / whitespace id is refused before the list is even fetched —
    /// and as a client mistake, not as an unreachable backend.
    #[tokio::test]
    async fn disconnect_refuses_a_blank_id_before_any_network_call() {
        // Unreachable backend — the argument check must fire first. If it did
        // not, this would surface as `Upstream`, which is what the assertion
        // below rules out.
        let err = delete_connection(&config("http://127.0.0.1:1", vec!["gmail".into()]), "  ")
            .await
            .expect_err("a blank id is refused");
        assert!(
            matches!(err, DisconnectError::NotFound(_)),
            "unexpected error: {err:?}"
        );
    }

    /// Issue #820: an account that is not usable cannot be the one agents act
    /// as. `c2` is a real gmail connection of this company's, and `INITIATED` —
    /// pinning it would route every gmail send to an account that cannot send,
    /// which is worse than the unpinned behaviour it replaces. So the refusal is
    /// a product decision, not a validation nicety, and it is asserted with the
    /// store: a refusal that still wrote would be a broken toolkit with a
    /// reassuring error message.
    ///
    /// The two blunter refusals share the test because they share the guard, and
    /// the assertion that matters for all three is the same one — nothing
    /// reached [`crate::company::composio::set_default`].
    #[tokio::test]
    async fn pinning_an_account_that_cannot_send_is_refused_and_stores_nothing() {
        use crate::company::composio::load_defaults;
        use crate::ports::types::CompanyId;
        use crate::store::FsSecretStore;

        let url = spawn_backend().await;
        let dir = tempfile::Builder::new()
            .prefix("oc-composio-pin-")
            .tempdir()
            .expect("tempdir");
        let secrets = FsSecretStore::new(dir.path());
        let company = CompanyId::new("acme");
        let cfg = config(&url, vec!["gmail".into(), "slack".into()]);

        let err = set_default_connection(&cfg, &company, &secrets, "c2")
            .await
            .expect_err("an account that is not connected cannot be pinned");
        // `NotFound` and not `Upstream`: the backend answered fine, and the
        // console must render this as the operator's mistake with the fix in it
        // ("re-authorize it"), not as a provider outage.
        assert!(
            matches!(err, DisconnectError::NotFound(_)),
            "unexpected error: {err:?}"
        );
        assert!(
            err.to_string().contains("INITIATED") && err.to_string().contains("not connected"),
            "the message names the status the operator has to fix: {err}"
        );

        // An id belonging to nobody, and an id belonging to this company under a
        // toolkit its manifest does not grant — the same boundary
        // `delete_connection` draws, so a pin cannot reach what no read shows.
        for id in ["nope", "c4", "   "] {
            match set_default_connection(&cfg, &company, &secrets, id).await {
                Err(DisconnectError::NotFound(_)) => {}
                other => panic!("`{id}` must be refused as NotFound, got {other:?}"),
            }
        }

        assert!(
            load_defaults(&company, &secrets)
                .await
                .expect("defaults read")
                .is_empty(),
            "a refused pin must not be stored — the whole point is that the next \
             agent turn is unchanged"
        );

        // The control: `c1` is the same toolkit, ACTIVE, and goes through. Without
        // it a guard that refused everything would pass every assertion above.
        let toolkit = set_default_connection(&cfg, &company, &secrets, "c1")
            .await
            .expect("an active account is pinnable");
        assert_eq!(toolkit, "gmail", "the pinned toolkit is reported back");
        assert_eq!(
            load_defaults(&company, &secrets)
                .await
                .expect("defaults read")
                .get("gmail")
                .map(String::as_str),
            Some("c1")
        );
    }

    /// The console's open-mode source (issue #397): the backend's real catalog,
    /// normalised. Connectable entries only, trimmed + lowercased, de-duplicated,
    /// sorted.
    #[tokio::test]
    async fn list_catalog_toolkits_returns_the_backends_connectable_catalog() {
        let url = spawn_backend().await;
        let catalog = list_catalog_toolkits(&config(&url, Vec::new()))
            .await
            .expect("catalog fetch");
        assert_eq!(
            catalog.iter().map(|e| e.slug.as_str()).collect::<Vec<_>>(),
            vec!["gmail", "hubspot"],
            "connectable entries only, normalised, de-duplicated and sorted"
        );
    }

    /// Issue #600: the display metadata the backend publishes reaches the
    /// caller instead of being reduced to a slug.
    ///
    /// This is the regression test for the defect itself. Every field asserted
    /// here was present in the response and discarded by a single
    /// `.map(|entry| entry.slug)`, which is why the console had nothing to
    /// group by, nothing to brand with, and nothing to search but the slug.
    #[tokio::test]
    async fn list_catalog_toolkits_carries_the_display_metadata() {
        let url = spawn_backend().await;
        let catalog = list_catalog_toolkits(&config(&url, Vec::new()))
            .await
            .expect("catalog fetch");

        let hubspot = catalog
            .iter()
            .find(|e| e.slug == "hubspot")
            .expect("hubspot is connectable");
        assert_eq!(hubspot.name, "HubSpot");
        assert_eq!(hubspot.description, "CRM and marketing automation.");
        assert_eq!(
            hubspot.logo.as_deref(),
            Some("https://logos.composio.dev/api/hubspot"),
            "the logo URL is what lets a tile be branded rather than a text row"
        );
        assert_eq!(
            hubspot.categories,
            vec!["crm".to_string(), "marketing".to_string()],
            "categories are trimmed and emptied-out entries dropped, but otherwise \
             forwarded verbatim — the console buckets them, not this layer"
        );

        let gmail = catalog
            .iter()
            .find(|e| e.slug == "gmail")
            .expect("gmail is connectable");
        assert_eq!(gmail.description, "Send and read email.");
        assert_eq!(
            gmail.logo, None,
            "an unpublished logo is None, not an empty string the console would \
             render as a broken image"
        );
        assert_eq!(
            gmail.name, "Gmail",
            "the FIRST entry for a slug wins, matching the de-duplication the slug \
             set used to do — not the later `Gmail (dup)`"
        );
    }

    /// A backend predating the dynamic catalog sends no `catalog[]`. Its plain
    /// slug allowlist is used rather than reporting an empty catalog — which the
    /// console would (correctly) render as a degraded fallback.
    #[tokio::test]
    async fn list_catalog_toolkits_falls_back_to_the_plain_allowlist() {
        let url = spawn_backend_with(get(legacy_toolkits_handler)).await;
        let catalog = list_catalog_toolkits(&config(&url, Vec::new()))
            .await
            .expect("catalog fetch");
        assert_eq!(
            catalog,
            vec![
                CatalogEntry::from_slug("gmail"),
                CatalogEntry::from_slug("notion"),
            ],
            "slug-only entries: the backend published nothing else, and the console \
             renders these with its own typography rather than dropping them"
        );
    }

    /// An unreachable backend is an error, never a quietly-empty catalog — the
    /// caller has to be able to tell "nothing is permitted" from "I could not
    /// ask".
    #[tokio::test]
    async fn list_catalog_toolkits_surfaces_a_fetch_failure() {
        let out = list_catalog_toolkits(&config("http://127.0.0.1:1", Vec::new())).await;
        out.expect_err("an unreachable backend must not read as an empty catalog");
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

        let dir = tempfile::Builder::new()
            .prefix("oc-composio-rot-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
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
