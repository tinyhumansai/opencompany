//! Per-tenant Composio connection management (issue #110, epic #26 Cell D): read
//! the company's Composio status and set a per-company OAuth bearer token.
//!
//! ## Where the credential normally comes from
//!
//! On the hosted platform a company needs to paste nothing. The instance already
//! authenticates with a platform-minted, audience-bound identity, and Composio
//! calls present that — the backend derives the Composio entity from it. The read
//! shape reports this as `credentialSource: "attested"`, and there is nothing
//! stored on the instance to leak, rotate, or lose.
//!
//! ## `PUT …/composio/token` — the BYO override
//!
//! The write route stays, and it is **not** the hosted path. It is:
//!
//! * the **per-company BYO override** — a company that has its own Composio
//!   account/token can set it here, and it wins over the instance identity for
//!   that company only; and
//! * the escape hatch for running this repo **standalone**. This repository is
//!   public and people do run it that way, but self-hosting is **unsupported**:
//!   there is no platform identity to present, so pasting a token is the only way
//!   to reach Composio at all. Do not read this as parity with the hosted
//!   product — it is a hatch, not a deployment mode, and nothing else about
//!   standalone operation is supported this milestone.
//!
//! ## Who a connection belongs to (issue #403)
//!
//! **A connection belongs to the company, and is admin-managed.** The account
//! reached through it is the account the company's *agents* act through, so it
//! is company property in the same sense the roster and the manifest are — not
//! the personal property of whichever member happened to click Connect.
//!
//! This is not a new position; it is the one the code already took and did not
//! enforce. The native OAuth plane's connect-time identity check
//! ([`account_mismatch`](super::connections)) compares the connected account
//! against `bootstrap_admins` — the company's *admin* addresses — which only
//! makes sense if a connection is the company's. Issue #316 settled "one
//! operator, one connected account"; this is the layer above it, and it
//! resolves the same way.
//!
//! Two consequences, both deliberate:
//!
//! * **Writes require an admin.** `PUT …/composio/token` and
//!   `POST …/composio/authorize` take [`AdminScopedCompany`]. A member is
//!   refused with a message that says why.
//! * **Reads stay open to any member.** `GET …/composio` and
//!   `GET …/composio/connections` carry no credential — only a tier name,
//!   non-secret routing, and which providers are connected. Knowing *that*
//!   Gmail is connected is what lets a member understand why an agent can read
//!   mail; being able to change it is the part that needed an owner. The split
//!   mirrors the roster, where `GET …/team` is open and the budget writes are
//!   not.
//!
//! Either way the token is **write-only** over the API: set through the `token`
//! field, stored in the secret store under
//! [`TOKEN_KEY`](crate::company::composio::TOKEN_KEY), and **never** echoed. The
//! read shape carries only `credentialSource` plus non-secret routing (backend
//! URL, toolkit allowlist) — never a token, and never a file path. A set / rotate
//! / clear takes effect on the agents' **next turn** with no restart (the harness
//! re-resolves the credential each turn and rebuilds the roster when the
//! *identity* behind it changes).

use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::routing::{delete, get, post, put};
use serde::{Deserialize, Serialize};

use crate::AppState;
// `token_configured` is gone from this list deliberately: the status route no
// longer re-derives the credential tier from booleans, it asks the resolver
// (`resolve_credential`) — see `credential_source_for` below.
use crate::company::composio::{
    CatalogEntry, ComposioMode, backend_url_or_default, resolve_access, resolve_credential,
    store_api_key, store_token,
};
use crate::company::composio_probe::{ComposioProbeClass, classify, describe, describe_verdict};
use crate::company::credentials::{CredentialSource, TinyhumansTokenSource};
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::CompanyEvent;
use crate::server::error::ApiError;
use crate::server::ops::composio_toolkits::{self, CatalogSource, OpenModeToolkits};
use crate::server::ops::{AdminScopedCompany, ScopedCompany, scoped};

/// The reminder attached to a set / rotate response.
const SWITCH_NOTE: &str =
    "Agents pick up the new Composio token on their next turn — no restart needed.";

/// The reminder attached to a clear response.
///
/// A clear does not hand agents a new token — it withdraws the BYO override, so
/// what they present next is whatever tier remains (a company key, the instance
/// identity, or none). Reusing [`SWITCH_NOTE`] here would tell the operator that
/// a new token exists when the effective credential has fallen back to nothing
/// (issue #1471).
const CLEAR_NOTE: &str =
    "Composio token cleared. Agents use whatever credential remains on their next turn.";

/// The reminder attached to storing a managed-route token for a company that is
/// **on BYOK**.
///
/// [`SWITCH_NOTE`] would be a lie here, in the way that matters most: it says
/// agents pick the token up on their next turn, and they do not. `resolve_access`
/// reads the BYOK key while the company is on that route, so a token stored for
/// the managed route sits there doing nothing until somebody chooses that route.
///
/// The state is reachable on purpose. A BYOK company whose managed chain
/// resolves to nothing cannot be offered "Use this" — that would be switching
/// into an outage — so the console offers the token first and the switch after,
/// which is the only order that works. Saying "in effect" at the end of the
/// first step would be reporting the second one as already done.
const INACTIVE_TOKEN_NOTE: &str = "Composio token saved for the managed route. This company is \
     still on its own Composio account, so agents keep using that key until you switch routes.";

/// The reminder attached to switching a company onto its own Composio account.
///
/// It names the consequence the radio button cannot: the providers connected
/// through the managed route live in the platform's Composio tenant and are not
/// visible from the company's own account, so the grid will look empty until
/// they are connected again there.
const BYOK_NOTE: &str = "This company now reaches Composio through its own account. Agents pick \
     that up on their next turn. Providers connected through TinyHumans-managed Composio stay in \
     that account — connect them again here, or clear the key to switch back.";

/// The reminder attached to giving the managed route back.
const MANAGED_NOTE: &str = "Composio API key cleared. This company is back on TinyHumans-managed \
     Composio from the agents' next turn, with the providers it had connected there.";

/// The toolkits the console should offer for a company, whether that answer
/// came from open mode, and where the list came from.
///
/// A non-empty manifest allowlist is authoritative and is offered **verbatim**:
/// a company that deliberately narrowed its belt sees exactly what it chose, the
/// backend catalog is not consulted, and nothing can widen it. That is not a
/// performance shortcut — it is the boundary. Widening a restrictive manifest
/// from a catalog fetch would silently hand a company providers it decided
/// against.
///
/// Empty means **open mode**, where the manifest is deferring to the backend's
/// own server-enforced allowlist. The honest answer there is the backend's live
/// catalog ([`composio_toolkits`]), fetched once per
/// [`CATALOG_TTL`](composio_toolkits::CATALOG_TTL) and marked
/// [`CatalogSource::Fallback`] with a reason if it cannot be had.
///
/// Returning the triple (rather than letting the console infer any of it) is the
/// whole point of the fix: the console must never have to guess which of two
/// opposite meanings an empty list carries, nor whether the list it is rendering
/// is the real catalog.
///
/// This is a **console affordance only**. Agent-side toolkit admission is
/// [`toolkit_allowed`](crate::harness::composio), which still treats an empty
/// manifest list as "allow every toolkit" — unchanged by any of this.
async fn effective_toolkits(
    runtime: &CompanyRuntime,
    manifest: &[String],
) -> (bool, OpenModeToolkits) {
    if manifest.is_empty() {
        (true, open_mode_toolkits(runtime).await)
    } else {
        (
            false,
            OpenModeToolkits {
                // A manifest allowlist is slugs and nothing else — the company
                // wrote it by hand, and the catalog is deliberately not
                // consulted here (it must not be able to widen a list that
                // narrowed on purpose). So these entries carry no metadata, and
                // the console falls back to its own typography for them.
                toolkits: manifest.iter().map(CatalogEntry::from_slug).collect(),
                source: CatalogSource::Manifest,
                notice: None,
            },
        )
    }
}

/// Open mode's provider list: the backend's live catalog, cached, or an
/// honestly-marked fallback.
///
/// The cache is consulted before the (cfg-split) fetch and records the outcome
/// either way, so a status poll during an outage costs a map lookup rather than
/// another fetch timeout of waiting. Concurrent callers on a cold key share one
/// fetch rather than each starting their own.
async fn open_mode_toolkits(runtime: &CompanyRuntime) -> OpenModeToolkits {
    let key = catalog_cache_key(runtime);
    let outcome = composio_toolkits::cache()
        .get_or_fetch(&key, || fetch_catalog(runtime))
        .await;
    OpenModeToolkits::from_outcome(outcome)
}

/// Drop this company's cached catalog.
///
/// Exposed because the catalog is a property of the **credential**, not of the
/// Composio token alone: changing the company's TinyHumans key
/// ([`ops::company_key`](super::company_key)) can change which account the
/// backend resolves and therefore which catalog it serves. Both writes evict
/// through here rather than each reaching into the cache with its own key
/// derivation.
pub(crate) fn evict_catalog_cache(runtime: &CompanyRuntime) {
    composio_toolkits::cache().evict(&catalog_cache_key(runtime));
}

/// This company's catalog cache key. The backend URL is resolved the same way
/// the status DTO resolves it, so a company repointed at a different backend
/// does not read the old backend's catalog.
fn catalog_cache_key(runtime: &CompanyRuntime) -> String {
    use crate::app::config::EnvSource;
    let env = crate::app::config::ProcessEnv;
    let backend_url = backend_url_or_default(
        env.get(crate::company::composio::COMPOSIO_BACKEND_URL_ENV),
        env.get(crate::company::composio::TINYHUMANS_API_URL_ENV),
    );
    composio_toolkits::cache_key(runtime.id(), &backend_url)
}

/// Fetch the backend's toolkit catalog for this company, bounded by
/// `composio_toolkits::FETCH_TIMEOUT`.
///
/// `Err` is a plain-language reason the console can show — never a bare
/// fall-through to a short list that would look authoritative.
#[cfg(feature = "composio")]
async fn fetch_catalog(runtime: &CompanyRuntime) -> Result<Vec<CatalogEntry>, String> {
    // No credential of any tier means there is nothing to dial the backend
    // with. Say that, rather than spending the timeout to discover it.
    //
    // But say only that when it is what happened. `resolve_tenant` answers
    // `Conflict` for "nothing configured" and propagates anything else — a
    // secret-store read failure among them. Collapsing every error into "no
    // credential yet" would tell an operator whose store hiccupped that they
    // never set a key, which is the confident-wrong-answer this credential work
    // exists to remove (see `company_key::resolve`).
    let config = resolve_tenant(runtime).await.map_err(|err| {
        if matches!(err.0, crate::error::OpenCompanyError::NotConfigured(_)) {
            "this company has no Composio credential yet, so the catalog cannot be read".to_string()
        } else {
            format!(
                "this company's Composio credential could not be resolved: {}",
                err.0
            )
        }
    })?;
    let fetch = crate::harness::composio::list_catalog_toolkits(&config);
    match tokio::time::timeout(composio_toolkits::FETCH_TIMEOUT, fetch).await {
        Err(_) => Err(format!(
            "the Composio backend did not answer within {}s",
            composio_toolkits::FETCH_TIMEOUT.as_secs()
        )),
        // `{err:#}`, not `to_string()`: the latter renders only the outermost
        // context and drops the cause chain behind it, so a catalog that failed
        // on a rejected API key reported the bare call name — "Composio v3
        // /toolkits" — with nothing saying what went wrong. The operator-facing
        // notice is the only place this surfaces, so it has to carry the reason.
        Ok(Err(err)) => Err(format!("{err:#}")),
        Ok(Ok(toolkits)) if toolkits.is_empty() => {
            Err("the Composio backend returned an empty catalog".to_string())
        }
        Ok(Ok(toolkits)) => Ok(toolkits),
    }
}

/// Without the `composio` feature there is no client to fetch a catalog with.
/// The status route still answers (reporting `inBuild:false`), and it says so
/// rather than presenting the fallback as the backend's list.
#[cfg(not(feature = "composio"))]
async fn fetch_catalog(_runtime: &CompanyRuntime) -> Result<Vec<CatalogEntry>, String> {
    Err("Composio is not compiled into this build".to_string())
}

/// Builds the Composio management route fragment.
///
/// The read/write-token plane (`GET …/composio`, `PUT …/composio/token`) is
/// always present. The per-provider OAuth sign-in plane (`POST
/// …/composio/authorize`, `GET …/composio/connections`, `DELETE
/// …/composio/connections/{connection_id}`) is **also** always present in the
/// route table — mirroring how `get_status` stays wired and reports
/// `inBuild:false` rather than `#[cfg]`-ing itself out — but its handlers only
/// reach the live Composio client under the `composio` feature; otherwise they
/// answer `409 not_in_build` "not in this build".
pub fn router() -> Router<AppState> {
    scoped("/composio", get(get_status))
        .merge(scoped("/composio/token", put(set_token)))
        .merge(scoped("/composio/api-key", put(set_api_key)))
        .merge(scoped("/composio/api-key/test", post(test_api_key)))
        .merge(scoped("/composio/authorize", post(authorize)))
        .merge(scoped("/composio/connections", get(connections)))
        .merge(scoped(
            "/composio/connections/{connection_id}",
            delete(disconnect),
        ))
        .merge(scoped(
            "/composio/connections/{connection_id}/default",
            put(set_default).delete(clear_default),
        ))
}

/// The company's Composio status as the console renders it. **Never** carries the
/// token — only the non-secret `credentialSource` plus routing.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ComposioStatusDto {
    /// Whether the `composio` feature is compiled into this build at all (the
    /// tools only exist under it). `false` lets the console show a "not in this
    /// build" state rather than implying a missing credential.
    in_build: bool,
    /// Whether the company **explicitly** grants the `composio` namespace (a `*`
    /// wildcard does NOT count).
    granted: bool,
    /// Where this company's Composio credential comes from:
    ///
    /// * `attested` — the instance's platform identity (nothing is stored here);
    /// * `static` — a token this company pasted, or a static instance key;
    /// * `none` — no credential can be obtained, so no tools are wired.
    ///
    /// A tier name, never a credential and never a path.
    credential_source: CredentialSource,
    /// Which tier the **managed** chain resolves to, whatever [`Self::mode`]
    /// says — the company's own backend token, then its TinyHumans key, then
    /// this instance's platform identity, then nothing.
    ///
    /// Under `managed` this is [`Self::credential_source`] by construction:
    /// both come from the same resolver, so they cannot disagree. Under `byok`
    /// they differ on purpose. `credentialSource` there names the **Composio
    /// API key** the calls actually present, because that is what the agents
    /// hold; this field names what the managed route *would* answer with if the
    /// key were cleared. Without it the console can show a BYOK company the
    /// consequence of going back to managed only by making it go back and look.
    ///
    /// A **tier name**. Never a credential, never a path, and — the part worth
    /// saying out loud — **never a boolean about a secret slot**. A
    /// `tokenConfigured`-shaped field was on this DTO once and was removed by
    /// issue #886: it answered "did somebody paste something into
    /// `composio/token`", which is one tier of the chain, and it read `false`
    /// for companies whose agents were calling `GITHUB_*` tools successfully in
    /// the same session. Do not reintroduce one under any name. The question it
    /// looked like it answered is answered here, by the tier that actually
    /// resolves.
    managed_credential_source: CredentialSource,
    /// Which host this company's Composio calls go to — `managed` (proxied
    /// through the OpenHuman backend, the default) or `byok` (straight to this
    /// company's own Composio account).
    ///
    /// Orthogonal to [`Self::credential_source`], which names *whose identity*
    /// a call presents rather than *which host* it is presented to. A BYOK
    /// company reports `byok` + `static`; a company that pasted a backend token
    /// override reports `managed` + `static`. Collapsing them into one field
    /// would leave the console unable to say which of the two an operator is
    /// looking at.
    mode: ComposioMode,
    /// The effective Composio backend URL (env override or default), or
    /// Composio's own API host under BYOK. Non-secret, and the address the
    /// calls really go to — a status line still naming the managed backend
    /// after a switch to BYOK would read as though nothing had happened.
    backend_url: String,
    /// The manifest toolkit allowlist verbatim (empty = defer to the backend
    /// allowlist, i.e. open mode). Kept as-is so an operator can still see what
    /// the manifest literally says; the console renders
    /// [`Self::effective_toolkits`] instead.
    toolkits: Vec<String>,
    /// Whether this company is in **open mode** — an empty manifest allowlist,
    /// meaning the backend's own allowlist governs and every toolkit it permits
    /// is reachable (issue #397).
    ///
    /// The console needs this told to it rather than inferred: an empty
    /// `toolkits` means *allow everything*, which is the opposite of what an
    /// empty list reads as.
    open_mode: bool,
    /// The toolkits the console offers as provider rows — the manifest list when
    /// it is non-empty, else the backend's live catalog (or, when that cannot be
    /// fetched, a built-in fallback flagged by [`Self::catalog_source`]). In open
    /// mode this is still not a hard limit: any slug the backend permits can be
    /// authorized by typing it.
    effective_toolkits: Vec<String>,
    /// The same providers as [`Self::effective_toolkits`], in the same order,
    /// carrying whatever display metadata the backend published for each —
    /// name, description, logo URL, and Composio's own category names (issue
    /// #600).
    ///
    /// **Additive, and deliberately so.** The slug list above is the contract
    /// every existing consumer reads and the only thing an authorize call
    /// needs; this is the render model beside it. Replacing the slug list with
    /// this one would have bought nothing and broken that contract.
    ///
    /// Empty metadata is a real state, not a bug: a manifest allowlist, a
    /// fallback list, and a backend predating the dynamic catalog all yield
    /// slug-only entries, and the console renders those with its own
    /// typography.
    ///
    /// The categories are forwarded **verbatim**, uninterpreted. The console
    /// buckets them by substring, which is what lets a Composio integration
    /// added tomorrow land in the right group with no change on either side of
    /// this wire.
    effective_catalog: Vec<CatalogEntry>,
    /// Where [`Self::effective_toolkits`] came from — `manifest`, `backend`, or
    /// `fallback` (issue #397).
    ///
    /// The console must be able to distinguish "these are the hundred providers
    /// the backend permits" from "the catalog could not be read, here are eight
    /// we know of". Both render as a list of slugs; only one of them is
    /// authoritative, and a console that could not tell them apart would present
    /// the second as though it were the first.
    catalog_source: CatalogSource,
    /// Why the list is a fallback, in plain language for the operator. `None`
    /// unless [`Self::catalog_source`] is `fallback`.
    catalog_notice: Option<String>,
}

/// A mutating response: the resulting status plus the switch reminder.
///
/// [`Self::advisory`] and [`Self::probe_class`] were added **additively** for
/// issue #2275 and are omitted entirely when there is nothing to say, so every
/// existing consumer of `status` + `note` reads the same body it always did.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResponse {
    status: ComposioStatusDto,
    note: String,
    /// What the credential check said, when the write landed anyway.
    ///
    /// Present only when a probe failed **non-destructively** — the key is
    /// stored and in use, and only its reachability is in question. Always
    /// [`describe`]'s fixed copy, never the upstream error text: that can echo
    /// request headers or a key fragment, and this lands in a banner somebody
    /// screenshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    advisory: Option<String>,
    /// The class behind [`Self::advisory`], so the console can decide how to
    /// render it (and whether to offer "add anyway") without parsing prose.
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_class: Option<ComposioProbeClass>,
}

/// Set-token body. `token` is write-only intake (never returned): a non-empty
/// value rotates the company's BYO override, an explicit empty string clears it
/// (reverting to the instance identity, where there is one).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetToken {
    token: String,
}

/// Set-API-key body. `apiKey` is write-only intake (never returned): a non-empty
/// value stores this company's own Composio API key and switches it to BYOK, an
/// explicit empty string clears the key and returns it to OpenHuman-managed
/// Composio.
///
/// One field, not two. A `mode` an operator could set independently of the key
/// would let them select BYOK with nothing stored — a company with no Composio
/// tools and no obvious reason why — so the mode is a consequence of the key
/// rather than a separate control. See
/// [`store_api_key`](crate::company::composio::store_api_key).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetApiKey {
    api_key: String,
    /// Store the key without checking it first.
    ///
    /// The escape hatch for a key that is valid but whose check cannot
    /// complete — a proxy in the way, a network the host cannot see past.
    /// Defaults to **false**, so a caller that has never heard of this field
    /// gets the checked path; nothing opts in by omission.
    ///
    /// The route simply honours it. Gating the *offer* on a probe having
    /// already failed is the console's job (see `connect-flow.md`: "add anyway"
    /// is unlocked by a typed probe failure and cleared on every retry), and it
    /// has to be, because a route cannot tell a considered override from a
    /// client that sends `true` always. What the route can guarantee is the
    /// default, which is what it guarantees.
    #[serde(default)]
    skip_verify: bool,
}

/// `POST …/composio/authorize` body: the toolkit slug (`gmail` / `slack` /
/// `github` / …) to begin an OAuth handoff for.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeBody {
    toolkit: String,
}

/// `POST …/composio/authorize` response: the Composio-hosted connect URL the
/// operator opens in a browser tab. Composio runs the OAuth itself; there is no
/// local callback — the console polls [`ConnectionDto`] until the toolkit
/// reports connected.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeDto {
    connect_url: String,
}

/// One per-toolkit connected state in the `GET …/composio/connections`
/// response: `connected` is true when the company has at least one active
/// connection for that toolkit.
///
/// [`accounts`](Self::accounts) was added for the provider detail view (issue
/// #404) **additively**: `toolkit` and `connected` keep their exact previous
/// meaning, so the tile grid and the post-authorize poll that read only those
/// two are untouched by it. A detail view therefore needs no second route and
/// no second round-trip — the call the page already makes now carries enough to
/// open a provider.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionDto {
    toolkit: String,
    connected: bool,
    /// Every connection this company holds for the toolkit, oldest id first.
    ///
    /// Usually one. Composio permits several accounts per toolkit, and a company
    /// that connected Gmail twice needs to see which is which before revoking
    /// one — the concrete reason `connected: bool` alone could not back a
    /// disconnect.
    accounts: Vec<ConnectedAccountDto>,
    /// The account this company chose for the toolkit, when it has chosen one
    /// (issue #820) — the id `composio_execute` sends as `connectionId`.
    ///
    /// **Omitted, not defaulted.** Absent means the company has expressed no
    /// intent and Composio resolves the account itself; there is no implicit
    /// default here to report, and inventing one (the oldest, the first in the
    /// sort) would be a claim the console makes and the harness does not honour.
    /// That absence is the honest state and stays the ordinary one.
    #[serde(skip_serializing_if = "Option::is_none")]
    default_connection_id: Option<String>,
}

/// One connected account inside a [`ConnectionDto`] (issue #404).
///
/// A non-secret projection of
/// [`ComposioConnectionRow`](crate::harness::composio::ComposioConnectionRow) —
/// see its docs for why the id is safe to hand the console.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectedAccountDto {
    /// Composio's connection id — the path segment `DELETE …/connections/{id}`
    /// takes.
    id: String,
    /// Composio's raw status string, forwarded verbatim so the console can tell
    /// "never set up" from "set up and expired".
    status: String,
    /// Whether this individual account is usable.
    connected: bool,
    /// When Composio recorded the connection, when it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    /// The account label, when the provider published one. Omitted rather than
    /// guessed — see the row type's docs.
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    /// Whether this is the account the company chose for the toolkit (issue
    /// #820). At most one account per toolkit carries it, and none does until
    /// somebody says so.
    is_default: bool,
}

/// How a company reaches Composio — the route, and which tier its credential
/// comes from.
///
/// Asks the resolver itself rather than restating its precedence. The console
/// must never be able to name a tier the agents are not on, and a second copy of
/// the rule is a second place to forget to update — which is how a status route
/// ends up confidently reporting a credential that no longer resolves.
///
/// Takes the instance identity already resolved, rather than an `&dyn
/// EnvSource`: a trait object with no `Send + Sync` bound held across the await
/// below makes the whole handler future non-`Send`, which axum rejects. Callers
/// resolve it from whichever environment they mean, so the matrix stays testable
/// without mutating the process environment.
async fn access_for(
    runtime: &CompanyRuntime,
    token_source: Option<std::sync::Arc<TinyhumansTokenSource>>,
) -> Result<(ComposioMode, CredentialSource), ApiError> {
    let access = resolve_access(runtime.id(), runtime.secrets().as_ref(), token_source)
        .await
        .map_err(ApiError)?;
    Ok((access.mode, access.credential.source()))
}

/// Resolves the Composio status DTO for a company.
async fn effective_status(runtime: &CompanyRuntime) -> Result<ComposioStatusDto, ApiError> {
    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let (granted, toolkits) = match record {
        Some(record) => (
            crate::company::grants_composio_explicit(&record.manifest.tools.allow),
            record.manifest.tools.composio.toolkits.clone(),
        ),
        None => (false, Vec::new()),
    };
    let env = crate::app::config::ProcessEnv;
    let (env_url, api_url) = {
        use crate::app::config::EnvSource;
        (
            env.get(crate::company::composio::COMPOSIO_BACKEND_URL_ENV),
            env.get(crate::company::composio::TINYHUMANS_API_URL_ENV),
        )
    };
    let token_source = TinyhumansTokenSource::from_env(&env).map(std::sync::Arc::new);
    let (mode, credential_source) = access_for(runtime, token_source.clone()).await?;
    // The managed chain, asked unconditionally.
    //
    // `access_for` short-circuits under BYOK — `resolve_access` reads the
    // company's Composio API key and stops — so `credential_source` there names
    // the BYOK key and says nothing about the route the company would return
    // to. Nor is the managed chain resolved anywhere else on this path: the
    // catalog fetch goes through `resolve_tenant`, which calls the same
    // `resolve_access` and therefore dials `backend.composio.dev` with the BYOK
    // key. So the managed tier has to be resolved here, explicitly, or not at
    // all.
    //
    // The cost is secret-store reads and no network — the same reads the
    // managed path already makes on every status poll — which is why this is
    // unconditional rather than `if mode.is_byok()`. One derivation, one code
    // path, and no branch where the field could come from somewhere else.
    let managed_credential_source =
        resolve_credential(runtime.id(), runtime.secrets().as_ref(), token_source)
            .await
            .map_err(ApiError)?
            .source();
    let (open_mode, effective) = effective_toolkits(runtime, &toolkits).await;
    // Report the host the calls actually reach. Under BYOK the managed backend
    // URL is resolved and then not used, so echoing it would be misdirection.
    let backend_url = match mode {
        ComposioMode::Byok => crate::company::composio::DIRECT_BASE_URL.to_string(),
        ComposioMode::Managed => backend_url_or_default(env_url, api_url),
    };
    Ok(ComposioStatusDto {
        in_build: cfg!(feature = "composio"),
        granted,
        credential_source,
        managed_credential_source,
        mode,
        backend_url,
        toolkits,
        open_mode,
        effective_toolkits: effective.slugs(),
        effective_catalog: effective.toolkits,
        catalog_source: effective.source,
        catalog_notice: effective.notice,
    })
}

/// `GET …/composio` — the company's Composio status.
async fn get_status(company: ScopedCompany) -> Result<Json<ComposioStatusDto>, ApiError> {
    Ok(Json(effective_status(company.runtime.as_ref()).await?))
}

/// `PUT …/composio/token` — set / rotate / clear this company's BYO token. See
/// the module docs: the hosted path needs no token, and standalone operation
/// (where this is the only option) is unsupported.
///
/// **Admin-only** (issue #403). This is the sharpest write in the module: the
/// token it stores is the identity every one of the company's agents presents
/// to Composio, so whoever sets it decides which account those agents act
/// through. That is a decision made *for* the company, not a member's own, and
/// [`AdminScopedCompany`] is what says so in the signature.
async fn set_token(
    company: AdminScopedCompany,
    Json(body): Json<SetToken>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    store_token(runtime.id(), runtime.secrets().as_ref(), &body.token)
        .await
        .map_err(ApiError)?;
    // The credential decides which Composio entity the backend resolves, so a
    // set / rotate / clear can change which catalog this company gets. Drop the
    // cached one rather than serving the previous account's answer for up to
    // `CATALOG_TTL` — the response below re-reads the status, so the operator
    // sees the new list immediately.
    evict_catalog_cache(runtime);
    // After the store, so the journal records a completed change. An empty
    // value is a clear, not a set — the two are worth telling apart in an
    // audit trail, since one grants access and the other withdraws it.
    let change = if body.token.trim().is_empty() {
        "credential_cleared"
    } else {
        "credential_set"
    };
    journal(&company, change, None).await?;
    // Read once and answered from, rather than read twice: the note below is a
    // statement about the same status this response carries, and deriving the
    // two from separate reads is how a page comes to show a sentence that
    // disagrees with the row underneath it.
    let status = effective_status(runtime).await?;
    let note = if body.token.trim().is_empty() {
        CLEAR_NOTE.to_string()
    } else if matches!(status.mode, ComposioMode::Managed) {
        SWITCH_NOTE.to_string()
    } else {
        // Stored for a route this company is not on. See `INACTIVE_TOKEN_NOTE`.
        INACTIVE_TOKEN_NOTE.to_string()
    };
    Ok(Json(MutationResponse {
        status,
        note,
        // Not probed. This route sets a bearer the *TinyHumans backend*
        // recognises, and there is no cheap call here that distinguishes a bad
        // bearer from a backend that is down — which is the distinction the
        // whole classifier exists to make. Checking it badly would be worse
        // than not checking it.
        advisory: None,
        probe_class: None,
    }))
}

/// `PUT …/composio/api-key` — bring this company's **own** Composio account, or
/// give it back.
///
/// A non-empty `apiKey` stores the key and routes every Composio call straight
/// to `backend.composio.dev` with it; an empty one clears the key and returns
/// the company to the OpenHuman-managed backend. This is the BYOK half of the
/// surface OpenHuman calls `direct` mode.
///
/// **Admin-only**, for the same reason [`set_token`] is: the key decides which
/// Composio account every one of the company's agents acts through, and — since
/// a BYOK account is billed to whoever owns it — who pays for the calls.
///
/// A switch in either direction is a **different tenant**: the providers
/// connected under the old route are not the ones connected under the new one,
/// so the console's cached catalog is dropped here exactly as a token change
/// drops it, and the response carries the freshly-read status so the operator
/// sees the new answer rather than the previous account's.
async fn set_api_key(
    company: AdminScopedCompany,
    Json(body): Json<SetApiKey>,
) -> Result<Json<MutationResponse>, ApiError> {
    let runtime = company.runtime.as_ref();
    let api_key = body.api_key.trim();
    // Probe the DRAFT, before anything is written. The clear path is never
    // probed — withdrawing a credential is always allowed, and there would be
    // nothing to check — and `skipVerify` is the operator's explicit opt-out.
    let probe = if api_key.is_empty() || body.skip_verify {
        None
    } else {
        classified_probe(runtime, api_key).await
    };
    // Composio rejected the key: nothing is stored, so the company's mode, its
    // existing key and its cached catalog are all exactly as they were — there
    // is no rollback here because there was no write. Not journaled either:
    // nothing about what this company connects through changed.
    if let Some(class) = probe
        && class.is_destructive()
    {
        return Err(ApiError(crate::error::OpenCompanyError::InvalidRequest(
            describe(class).to_string(),
        )));
    }
    let mode = store_api_key(runtime.id(), runtime.secrets().as_ref(), api_key)
        .await
        .map_err(ApiError)?;
    evict_catalog_cache(runtime);
    journal(
        &company,
        match mode {
            ComposioMode::Byok => "composio_byok_set",
            ComposioMode::Managed => "composio_byok_cleared",
        },
        None,
    )
    .await?;
    Ok(Json(MutationResponse {
        status: effective_status(runtime).await?,
        note: match mode {
            ComposioMode::Byok => BYOK_NOTE.to_string(),
            ComposioMode::Managed => MANAGED_NOTE.to_string(),
        },
        // A non-destructive class rode through the store: the key is live, and
        // only the check is in doubt.
        advisory: probe.map(|class| describe(class).to_string()),
        probe_class: probe,
    }))
}

/// Ask Composio whether it recognises an API key, and classify the answer.
/// `None` means the check came back clean.
///
/// Two callers, one derivation: [`set_api_key`] checks a **draft** before
/// storing it, and [`test_api_key`] checks the one already **stored**. Both
/// hand the key in directly, so neither needs the key to be resolvable from
/// anywhere, and a second copy of this would be a second place for the two
/// routes' verdicts to drift apart.
///
/// The handlers are auth, shape and delegation; every branch worth a test is
/// either here (one `match`) or in
/// [`composio_probe`](crate::company::composio_probe), which is pure. What this
/// function owns is the one rule that cannot live in either: **the raw upstream
/// reason goes to `debug` and nowhere else.** It can carry a proxy's HTML, a
/// response header, or a key fragment, and the only string that leaves this
/// function is the class.
async fn classified_probe(runtime: &CompanyRuntime, api_key: &str) -> Option<ComposioProbeClass> {
    #[cfg(test)]
    let outcome = match probe_override::get(runtime.id().as_ref()) {
        Some(forced) => forced,
        None => probe_transport(api_key).await,
    };
    #[cfg(not(test))]
    let outcome = probe_transport(api_key).await;

    match outcome {
        Ok(()) => None,
        Err(raw) => {
            let class = classify(&raw);
            tracing::debug!(
                company = %runtime.id(),
                class = %class,
                error = %raw,
                "[composio] a draft API key did not check out"
            );
            Some(class)
        }
    }
}

/// The probe's network call, split on the feature exactly as [`fetch_catalog`]
/// is.
#[cfg(feature = "composio")]
async fn probe_transport(api_key: &str) -> Result<(), String> {
    crate::harness::composio_direct::probe_api_key(api_key).await
}

/// Without the `composio` feature there is no client to check a key with.
///
/// This reports the key as **un-probeable**, which classifies `unknown` — the
/// non-destructive class — so the key is stored with an advisory. That is the
/// same shape [`fetch_catalog`] takes (an honest `Err` that the caller degrades
/// gracefully from) and it is the only safe direction: refusing the write would
/// make a default build unable to configure BYOK at all, and treating an
/// absent client as a rejected credential would throw away a key nothing ever
/// looked at.
#[cfg(not(feature = "composio"))]
async fn probe_transport(_api_key: &str) -> Result<(), String> {
    Err("Composio is not compiled into this build, so the key could not be checked".to_string())
}

/// Test-only: force [`classified_probe`]'s transport answer for one company.
///
/// The route's three outcomes — rejected, clean, advisory — are the whole point
/// of the change, and two of them cannot be reached from a default build (which
/// has no Composio client) nor from a feature build (which would dial
/// `backend.composio.dev` from a unit test). Keyed by company id rather than
/// held as one global, for the reason `composio_toolkits::cache` is: the test
/// binary runs these concurrently in one process, and a single slot would make
/// them race. `#[cfg(test)]` throughout — there is no seam here in a shipped
/// build.
#[cfg(test)]
mod probe_override {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    type Outcome = Result<(), String>;

    fn map() -> &'static Mutex<HashMap<String, Outcome>> {
        static MAP: OnceLock<Mutex<HashMap<String, Outcome>>> = OnceLock::new();
        MAP.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) fn set(company: &str, outcome: Outcome) {
        map()
            .lock()
            .expect("composio probe override")
            .insert(company.to_string(), outcome);
    }

    pub(super) fn get(company: &str) -> Option<Outcome> {
        map()
            .lock()
            .expect("composio probe override")
            .get(company)
            .cloned()
    }
}

/// `POST …/composio/api-key/test` — check the **stored** Composio API key and
/// report the verdict. Changes nothing.
///
/// ## It never writes, on any path — including `auth`
///
/// Not even a rejected key is cleared here, and this is the comment that exists
/// so a later edit does not "tidy" this into reuse of [`set_api_key`]'s path.
/// Testing a credential and committing to (or withdrawing from) it are separate
/// acts — the same rule `POST /api/v1/setup/inference/test` already states — and
/// a Test button that deleted a company's key on a bad afternoon would be the
/// worst control on the page: the operator pressed the one thing that promised
/// to be safe. A key an operator wants gone goes through `PUT …/composio/api-key`
/// with an empty value, deliberately, which is also the only path that journals
/// the change.
///
/// Consequently there is no journal line here either. Nothing about what this
/// company connects through changed, and an audit trail that records reads
/// dilutes the one thing it is for.
///
/// ## No body, and there never will be one
///
/// The key is read from the company's own store and the endpoint is the
/// compile-time [`DIRECT_BASE_URL`](crate::company::composio::DIRECT_BASE_URL).
/// A body carrying a key — or worse, an endpoint — would turn an authenticated
/// console route into "send this credential to that host", which is the
/// SSRF-shaped primitive the constant destination currently rules out. A draft
/// key is checked by [`set_api_key`], which has to be handed one anyway.
///
/// ## Admin-only
///
/// It performs no write, so this is not the usual write/read split. It spends
/// the company's own credential against a third party and reports whether that
/// credential is good — an action taken on the company's behalf, and one whose
/// answer (`this company's Composio account is rejected`) is about the
/// company's standing with a vendor rather than about what its agents can do.
/// Reads stay open to members because they carry no credential and trigger no
/// outbound call; this one does the second of those.
async fn test_api_key(company: AdminScopedCompany) -> Result<Json<ApiKeyTestDto>, ApiError> {
    let runtime = company.runtime.as_ref();
    let api_key = stored_api_key(runtime).await?;
    // `NotConfigured`, not the catch-all conflict: "there is nothing here to
    // test" is a permanent state the console reads off the code and renders as
    // a disabled control, and it must stay distinguishable from a check that
    // ran and failed. Same code `resolve_tenant` answers with, for the same
    // distinction.
    let Some(api_key) = api_key else {
        return Err(ApiError(crate::error::OpenCompanyError::NotConfigured(
            "this company is on TinyHumans-managed Composio, so there is no API key to test — \
             paste one to bring its own Composio account"
                .to_string(),
        )));
    };
    Ok(Json(match classified_probe(runtime, &api_key).await {
        None => ApiKeyTestDto {
            ok: true,
            probe_class: None,
            message: None,
        },
        Some(class) => ApiKeyTestDto {
            ok: false,
            probe_class: Some(class),
            // `describe_verdict`, not `describe`: this route stored nothing, and
            // the latter's copy says `Saved, …`.
            message: Some(describe_verdict(class).to_string()),
        },
    }))
}

/// The company's own Composio API key, or `None` when there is nothing to
/// check.
///
/// `None` covers both ways that happens — the company is on the managed route,
/// or it is on BYOK with a blank slot — because the answer the caller gives is
/// the same either way and a second variant would only invite a second
/// sentence. The BYOK-with-no-key case is already a state
/// [`resolve_access`] warns about on the agent path.
async fn stored_api_key(runtime: &CompanyRuntime) -> Result<Option<String>, ApiError> {
    use crate::company::composio::{API_KEY_KEY, load_mode};

    if !load_mode(runtime.id(), runtime.secrets().as_ref())
        .await
        .map_err(ApiError)?
        .is_byok()
    {
        return Ok(None);
    }
    let stored = runtime
        .secrets()
        .get(runtime.id(), API_KEY_KEY)
        .await
        .map_err(ApiError)?;
    Ok(stored
        .map(|crate::ports::types::SecretValue(key)| key.trim().to_string())
        .filter(|key| !key.is_empty()))
}

/// `POST …/composio/api-key/test` response — a verdict, and never a credential.
///
/// [`Self::ok`] alone answers the console's control. The other two are present
/// only on a failure: the class so the page can decide how to render it, and
/// the fixed copy for the class so it does not have to hold its own table of
/// sentences. Both are omitted rather than nulled, matching every other
/// optional field on this surface.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiKeyTestDto {
    /// Whether Composio answered the check.
    ok: bool,
    /// Why it did not, when it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_class: Option<ComposioProbeClass>,
    /// The operator-facing sentence for [`Self::probe_class`] — always
    /// [`describe_verdict`]'s fixed copy, never the upstream error text.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Records who changed the company's tool access (issue #403).
///
/// Propagates a journal failure rather than swallowing it, unlike the
/// best-effort workflow journaling. The point of this record is that a change
/// to what the company's agents connect through is never invisible; an audit
/// line that quietly fails to be written is the one failure mode that would
/// defeat it. `MemoryFactDeleted` — the other write journaled *for* the audit
/// trail — propagates for the same reason.
async fn journal(
    company: &AdminScopedCompany,
    change: &str,
    toolkit: Option<String>,
) -> Result<(), ApiError> {
    company
        .runtime
        .events()
        .append(
            company.id(),
            CompanyEvent::ToolAccessChanged {
                change: change.to_string(),
                toolkit,
                by: Some(company.actor()),
            },
        )
        .await
        .map_err(ApiError)?;
    Ok(())
}

/// `POST …/composio/authorize` — begin a per-provider OAuth handoff and return
/// the Composio-hosted connect URL the operator opens in a browser tab.
///
/// Composio runs the OAuth flow itself — there is **no** local callback route.
/// The console opens the URL and polls [`connections`] until the toolkit
/// reports connected. Under a non-`composio` build this answers `409
/// not_in_build` (see [`router`]).
///
/// **Admin-only** (issue #403). The connection this begins belongs to the
/// company — it is the account its agents will act through from then on — so
/// the person who chooses it has to be someone entitled to choose on the
/// company's behalf. Note that nothing downstream can re-check this: Composio
/// runs its own OAuth with no callback here, so unlike the native connections
/// plane there is no later point at which the connecting identity could be
/// compared against the company's operators. Connect time is the only boundary
/// there is.
async fn authorize(
    company: AdminScopedCompany,
    Json(body): Json<AuthorizeBody>,
) -> Result<Json<AuthorizeDto>, ApiError> {
    let dto = authorize_impl(company.runtime.as_ref(), body.toolkit.clone()).await?;
    // Only once a connect URL was actually minted — a refused or unbuildable
    // authorize changed nothing and does not belong in the trail.
    journal(
        &company,
        "provider_authorization_started",
        Some(body.toolkit),
    )
    .await?;
    Ok(dto)
}

/// `GET …/composio/connections` — the company's per-toolkit connected state,
/// one [`ConnectionDto`] per toolkit that has at least one connection. The
/// console cross-references this against the granted `toolkits` to render each
/// provider row's connected/sign-in state. `409 not_in_build` on a
/// non-`composio` build, and `409 not_configured` when this company has no
/// Composio credential — two permanent states the console reads off the code,
/// because neither is cleared by trying the read again.
async fn connections(company: ScopedCompany) -> Result<Json<Vec<ConnectionDto>>, ApiError> {
    connections_impl(company.runtime.as_ref()).await
}

/// Resolve the per-tenant Composio config (bearer + backend URL + toolkit
/// allowlist) the way the harness roster build does, so the console dials the
/// backend with the exact same tenant identity. A `409 not_configured` when no
/// credential of any tier can be resolved — neither this company's own stored
/// token nor a platform identity — in which case the operator must paste a token
/// before OAuth. Its own code, not the catch-all `conflict`: a caller has to be
/// able to tell "nothing is set up here" from a write that lost a race.
#[cfg(feature = "composio")]
pub(crate) async fn resolve_tenant(
    runtime: &CompanyRuntime,
) -> Result<crate::harness::composio::TenantComposio, ApiError> {
    use crate::app::config::EnvSource;

    let record = runtime.store().load(runtime.id()).await.map_err(ApiError)?;
    let toolkits = record
        .map(|record| record.manifest.tools.composio.toolkits.clone())
        .unwrap_or_default();
    let env = crate::app::config::ProcessEnv;
    let backend_env = env.get(crate::company::composio::COMPOSIO_BACKEND_URL_ENV);
    let api_env = env.get(crate::company::composio::TINYHUMANS_API_URL_ENV);
    // Resolved here rather than through `TenantComposio::resolve` so a store
    // read failure stays distinguishable from "nothing configured". This is the
    // path that *establishes* a connection, and a connection lives on the
    // backend keyed by the account the bearer resolves to — so guessing here
    // would attribute a company's Gmail to whichever identity happened to
    // resolve during the outage. The roster path can afford to shrug and
    // withhold tools; this one must say what went wrong and connect nothing.
    let access = resolve_access(
        runtime.id(),
        runtime.secrets().as_ref(),
        crate::company::TinyhumansTokenSource::from_env(&env).map(std::sync::Arc::new),
    )
    .await
    .map_err(ApiError)?;
    if !access.credential.configured() {
        // The two routes fail for different reasons and are recoverable in
        // different places, so they say different things. A BYOK company told
        // to "set its TinyHumans credential" would be sent to a control that
        // has no effect on the route it chose.
        return Err(ApiError(crate::error::OpenCompanyError::NotConfigured(
            match access.mode {
                ComposioMode::Byok => "this company uses its own Composio account but no Composio \
                     API key is stored — paste one, or clear it to go back to TinyHumans-managed \
                     Composio"
                    .to_string(),
                ComposioMode::Managed => "no Composio credential is available for this company — \
                     set the company's TinyHumans credential, or paste its own Composio token"
                    .to_string(),
            },
        )));
    }
    Ok(crate::harness::composio::TenantComposio::from_access(
        backend_url_or_default(backend_env, api_env),
        access,
        toolkits,
    ))
}

#[cfg(feature = "composio")]
async fn authorize_impl(
    runtime: &CompanyRuntime,
    toolkit: String,
) -> Result<Json<AuthorizeDto>, ApiError> {
    let config = resolve_tenant(runtime).await?;
    let connect_url = crate::harness::composio::authorize_connect_url(&config, &toolkit)
        .await
        .map_err(|err| {
            ApiError(crate::error::OpenCompanyError::TinyHumans {
                code: "composio_authorize".to_string(),
                message: err.to_string(),
            })
        })?;
    Ok(Json(AuthorizeDto { connect_url }))
}

#[cfg(feature = "composio")]
async fn connections_impl(runtime: &CompanyRuntime) -> Result<Json<Vec<ConnectionDto>>, ApiError> {
    let config = resolve_tenant(runtime).await?;
    let rows = crate::harness::composio::list_connections_detailed(&config)
        .await
        .map_err(|err| {
            ApiError(crate::error::OpenCompanyError::TinyHumans {
                code: "composio_connections".to_string(),
                message: err.to_string(),
            })
        })?;
    let defaults =
        crate::company::composio::load_defaults(runtime.id(), runtime.secrets().as_ref())
            .await
            .map_err(ApiError)?;
    let defaults = drop_dangling_defaults(runtime, &rows, defaults).await?;
    Ok(Json(group_by_toolkit(rows, &defaults)))
}

/// Drop every stored choice naming a connection `rows` does not contain, and
/// return what is left (issue #820).
///
/// Housekeeping on the one path that holds both halves of the answer. Such a pin
/// is not merely stale — it would be sent on the next `composio_execute` and
/// refused, so an account revoked outside this console (at Composio itself)
/// would silently break the toolkit for every agent. The console polls this
/// route, so the repair lands on its own. Deliberately a *removal of a dangling
/// reference* and nothing else: it cannot pick a different account, only stop
/// naming one that is gone.
///
/// Split out from the handler for the same reason [`group_by_toolkit`] is: the
/// decision is testable without standing up a Composio backend, and the rows a
/// test has to invent are exactly the ones the handler was handed.
#[cfg(feature = "composio")]
async fn drop_dangling_defaults(
    runtime: &CompanyRuntime,
    rows: &[crate::harness::composio::ComposioConnectionRow],
    defaults: crate::company::composio::ComposioDefaults,
) -> Result<crate::company::composio::ComposioDefaults, ApiError> {
    let live: std::collections::BTreeSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    // Iterated over a snapshot, one write per stale toolkit, and `defaults` is
    // reassigned from each write rather than mutated here: `clear_default`
    // re-reads the stored blob and returns the whole map as it now stands, so
    // the last iteration's return is the fully reduced map and the answer this
    // returns cannot drift from what was actually persisted. With nothing stale
    // the loop does not run and the map passed in is returned untouched — no
    // write on the ordinary read.
    let mut defaults = defaults;
    for (toolkit, id) in defaults
        .clone()
        .into_iter()
        .filter(|(_, id)| !live.contains(id.as_str()))
    {
        tracing::info!(
            company = %runtime.id(),
            toolkit = %toolkit,
            connection_id = %id,
            "[composio] the chosen account no longer exists at Composio; clearing the choice"
        );
        defaults = crate::company::composio::clear_default(
            runtime.id(),
            runtime.secrets().as_ref(),
            &toolkit,
        )
        .await
        .map_err(ApiError)?;
    }
    Ok(defaults)
}

/// Fold per-connection rows into the per-toolkit response shape.
///
/// `connected` is `true` when **any** account for the toolkit is active — the
/// same rule the pre-#404 route applied, kept here so the boolean the tile grid
/// reads cannot drift from what it meant before the accounts were added.
///
/// A `BTreeMap` gives the toolkit ordering the route has always had; the rows
/// arrive already sorted by `(toolkit, id)`, so the per-toolkit account order is
/// stable too. Split out from the handler so it is testable without a live
/// Composio backend.
#[cfg(feature = "composio")]
fn group_by_toolkit(
    rows: Vec<crate::harness::composio::ComposioConnectionRow>,
    defaults: &crate::company::composio::ComposioDefaults,
) -> Vec<ConnectionDto> {
    let mut by_toolkit: std::collections::BTreeMap<String, ConnectionDto> =
        std::collections::BTreeMap::new();
    for row in rows {
        let chosen = defaults.get(&row.toolkit).map(String::as_str);
        let entry = by_toolkit
            .entry(row.toolkit.clone())
            .or_insert_with(|| ConnectionDto {
                toolkit: row.toolkit.clone(),
                connected: false,
                accounts: Vec::new(),
                default_connection_id: chosen.map(str::to_string),
            });
        entry.connected = entry.connected || row.connected;
        entry.accounts.push(ConnectedAccountDto {
            is_default: chosen == Some(row.id.as_str()),
            id: row.id,
            status: row.status,
            connected: row.connected,
            created_at: row.created_at,
            account: row.account,
        });
    }
    by_toolkit.into_values().collect()
}

/// `DELETE …/composio/connections/{id}` — revoke one connected account
/// (issue #404).
///
/// **Admin-only** (issue #403), for the same reason `authorize` is: the
/// connection belongs to the company, so removing the account its agents act
/// through is a decision made on the company's behalf. Journaled on success
/// only, alongside the connect it reverses.
///
/// What this does and does not revoke matters, and the console says so before
/// asking: it removes the connection **at Composio**, so agents lose the
/// capability on their next turn. It does not sign the company out of the
/// provider, and it does not touch the native `oauth/{provider}` catalog entry,
/// which is a separate credential this plane has never owned.
async fn disconnect(
    company: AdminScopedCompany,
    Path(ConnectionPath { connection_id }): Path<ConnectionPath>,
) -> Result<Json<DisconnectDto>, ApiError> {
    let dto = disconnect_impl(company.runtime.as_ref(), &connection_id).await?;
    journal(&company, "provider_disconnected", None).await?;
    Ok(dto)
}

/// The sub-resource path (`connection_id`); the scope `id` is consumed by the
/// extractor.
#[derive(Debug, Deserialize)]
struct ConnectionPath {
    connection_id: String,
}

/// `DELETE …/composio/connections/{id}` response. A body rather than a bare 204
/// so the console can state what happened in the same words the host used.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DisconnectDto {
    /// Plain-language confirmation, scoped to what was actually revoked.
    note: String,
}

#[cfg(feature = "composio")]
async fn disconnect_impl(
    runtime: &CompanyRuntime,
    connection_id: &str,
) -> Result<Json<DisconnectDto>, ApiError> {
    use crate::harness::composio::DisconnectError;

    let config = resolve_tenant(runtime).await?;
    crate::harness::composio::delete_connection(&config, connection_id)
        .await
        // An id this company cannot see is a `404`, not a `502`. Both were the
        // latter until the route was actually run: the console would have told
        // an operator the provider was unreachable about a call that never left
        // the host, and a retry — the obvious response to a bad gateway — would
        // fail identically forever.
        .map_err(|err| match err {
            DisconnectError::NotFound(message) => {
                ApiError(crate::error::OpenCompanyError::NotFound(message))
            }
            DisconnectError::Upstream(err) => {
                ApiError(crate::error::OpenCompanyError::TinyHumans {
                    code: "composio_disconnect".to_string(),
                    message: err.to_string(),
                })
            }
        })?;
    // A pin naming the account just revoked would be sent on the next execute
    // and refused — so disconnecting the account a company *did not* choose
    // must not be what breaks the one it did (issue #820).
    crate::company::composio::forget_connection(
        runtime.id(),
        runtime.secrets().as_ref(),
        connection_id,
    )
    .await
    .map_err(ApiError)?;
    Ok(Json(DisconnectDto {
        note: "Disconnected at Composio. Agents lose these tools on their next turn.".to_string(),
    }))
}

#[cfg(not(feature = "composio"))]
async fn disconnect_impl(
    _runtime: &CompanyRuntime,
    _connection_id: &str,
) -> Result<Json<DisconnectDto>, ApiError> {
    Err(not_in_build())
}

/// `PUT …/composio/connections/{id}/default` — make that account the one this
/// company's agents act as for its toolkit (issue #820).
///
/// **Admin-only** (issue #403), for the reason `authorize` and `disconnect` are:
/// "send from billing@, not ops@" is a decision about what the company does, not
/// a per-operator preference — every agent in the company acts through the one
/// answer.
///
/// The account is named by **connection id**, not by toolkit-plus-id, because
/// the toolkit is already a property of the connection: asking the caller to
/// repeat it would invite the two to disagree, and the id alone is what the
/// console has in hand from `GET …/connections`.
async fn set_default(
    company: AdminScopedCompany,
    Path(ConnectionPath { connection_id }): Path<ConnectionPath>,
) -> Result<Json<DefaultDto>, ApiError> {
    let dto = set_default_impl(company.runtime.as_ref(), &connection_id).await?;
    journal(
        &company,
        "provider_default_account_set",
        Some(dto.0.toolkit.clone()),
    )
    .await?;
    Ok(dto)
}

/// `DELETE …/composio/connections/{id}/default` — stop naming an account for
/// that connection's toolkit, returning it to Composio's own resolution.
///
/// Unlike [`set_default`] this makes **no upstream call** and validates nothing
/// against Composio: the whole point of clearing is to be able to undo a pin
/// when the account is gone or the provider is unreachable, which is exactly
/// when a validating clear would refuse. It removes any pin naming this id and
/// says so; a request for an id that was never pinned is a no-op, not an error.
async fn clear_default(
    company: AdminScopedCompany,
    Path(ConnectionPath { connection_id }): Path<ConnectionPath>,
) -> Result<Json<DefaultDto>, ApiError> {
    let cleared = crate::company::composio::forget_connection(
        company.runtime.id(),
        company.runtime.secrets().as_ref(),
        &connection_id,
    )
    .await
    .map_err(ApiError)?;
    if cleared {
        journal(&company, "provider_default_account_cleared", None).await?;
    }
    Ok(Json(DefaultDto {
        toolkit: String::new(),
        connection_id: None,
        note: if cleared {
            "Cleared. Composio picks the account for this provider again, as it did before."
                .to_string()
        } else {
            "That account was not the default; nothing changed.".to_string()
        },
    }))
}

/// The `…/default` response: what is now pinned, and a sentence saying so.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DefaultDto {
    /// The toolkit the change applied to. Empty on a clear, where the caller
    /// named a connection rather than a toolkit and may be clearing a pin for
    /// an account that no longer exists.
    toolkit: String,
    /// The account now acting for that toolkit — `None` after a clear.
    #[serde(skip_serializing_if = "Option::is_none")]
    connection_id: Option<String>,
    /// Plain-language confirmation, in the same words the console repeats.
    note: String,
}

#[cfg(feature = "composio")]
async fn set_default_impl(
    runtime: &CompanyRuntime,
    connection_id: &str,
) -> Result<Json<DefaultDto>, ApiError> {
    use crate::harness::composio::DisconnectError;

    let config = resolve_tenant(runtime).await?;
    let toolkit = crate::harness::composio::set_default_connection(
        &config,
        runtime.id(),
        runtime.secrets().as_ref(),
        connection_id,
    )
    .await
    // Same split as `disconnect`: an id this company cannot see is a `404`
    // about a call that never left the host, not a `502` about a provider that
    // is up.
    .map_err(|err| match err {
        DisconnectError::NotFound(message) => {
            ApiError(crate::error::OpenCompanyError::NotFound(message))
        }
        DisconnectError::Upstream(err) => ApiError(crate::error::OpenCompanyError::TinyHumans {
            code: "composio_set_default".to_string(),
            message: err.to_string(),
        }),
    })?;
    Ok(Json(DefaultDto {
        note: format!(
            "Agents act as this account for {toolkit} from their next turn. Other accounts stay \
             connected."
        ),
        toolkit,
        connection_id: Some(connection_id.to_string()),
    }))
}

#[cfg(not(feature = "composio"))]
async fn set_default_impl(
    _runtime: &CompanyRuntime,
    _connection_id: &str,
) -> Result<Json<DefaultDto>, ApiError> {
    Err(not_in_build())
}

/// A `409 not_in_build` "Composio is not in this build" — the OAuth plane's
/// off-state under a non-`composio` build. Mirrors the status route's
/// `inBuild:false` semantics rather than pretending nothing is connected.
///
/// The code is what the console keys on. Under the catch-all `conflict` this
/// answer was indistinguishable on the wire from a lost publish race, so the
/// only reading available to a caller was the recoverable one — and the Apps
/// page told every operator on a default build to reload a section no reload
/// could ever fill.
#[cfg(not(feature = "composio"))]
fn not_in_build() -> ApiError {
    ApiError(crate::error::OpenCompanyError::NotInBuild(
        "Composio is not compiled into this build".to_string(),
    ))
}

#[cfg(not(feature = "composio"))]
async fn authorize_impl(
    _runtime: &CompanyRuntime,
    _toolkit: String,
) -> Result<Json<AuthorizeDto>, ApiError> {
    Err(not_in_build())
}

#[cfg(not(feature = "composio"))]
async fn connections_impl(_runtime: &CompanyRuntime) -> Result<Json<Vec<ConnectionDto>>, ApiError> {
    Err(not_in_build())
}

#[cfg(test)]
mod tests {
    use super::{
        CatalogEntry, CatalogSource, ComposioMode, ComposioStatusDto, CredentialSource,
        TinyhumansTokenSource, access_for,
    };
    use crate::company::runtime::CompanyRuntime;
    use crate::server::error::ApiError;
    use crate::server::ops::composio_toolkits;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    #[cfg(feature = "composio")]
    use axum::routing::post;
    #[cfg(feature = "composio")]
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::types::{CompanyId, CompanyRecord};
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    const TOKEN: &str = "composio-tenant-bearer-SECRET-xyz";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-composio-")
            .tempdir()
            .expect("tempdir")
    }

    /// A loopback Composio authorize endpoint for the feature-enabled route
    /// test. Keeping the HTTP boundary real proves the handler reaches the
    /// client after the admin guard, without allowing a unit test to dial the
    /// production backend.
    #[cfg(feature = "composio")]
    async fn spawn_authorize_backend() -> String {
        let app = Router::new().route(
            "/agent-integrations/composio/authorize",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["toolkit"], "gmail");
                Json(json!({
                    "success": true,
                    "data": {
                        "connectUrl": "https://composio.test/connect/gmail",
                        "connectionId": "gmail-connection"
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    async fn state_with_manifest(home: &std::path::Path, manifest_toml: &str) -> AppState {
        state_with_manifest_id(home, "acme", manifest_toml).await
    }

    /// The same, under an explicit company id.
    ///
    /// The catalog cache is process-wide and keyed by company, so the tests that
    /// seed it need ids of their own — two tests sharing `acme` would share an
    /// entry and race.
    async fn state_with_manifest_id(
        home: &std::path::Path,
        company: &str,
        manifest_toml: &str,
    ) -> AppState {
        use crate::ports::CompanyStore;
        let manifest: CompanyManifest = toml::from_str(manifest_toml).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new(company);
        store
            .save(&CompanyRecord {
                overlay_desk_hive: Vec::new(),
                overlay_retired_agents: Vec::new(),
                overlay_agent_edits: Vec::new(),
                id: id.clone(),
                manifest: manifest.clone(),
                ledger: Vec::new(),
                lifecycle: "running".to_string(),
                overlay_agents: Vec::new(),
                overlay_desk_members: Vec::new(),
                overlay_desk_order: Vec::new(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .unwrap();
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id.clone())
            .build()
            .await
            .unwrap();
        let state = AppState::new(AppConfig::default());
        state.registry().insert(id, std::sync::Arc::new(runtime));
        crate::server::test_support::seed_fixed_admin(&state, company).await;
        state
    }

    async fn send(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, String) {
        send_for(state, "acme", method, uri, body).await
    }

    /// [`send`] against a named company's session.
    async fn send_for(
        state: &AppState,
        company: &str,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, String) {
        send_as(
            state,
            method,
            uri,
            body,
            Auth::Cookie(crate::server::test_support::fixed_cookie(company)),
        )
        .await
    }

    /// How a request presents itself, so the role boundary can be driven with
    /// an admin session, a member session, or a machine credential.
    enum Auth {
        Cookie(String),
        Bearer(String),
    }

    async fn send_as(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
        auth: Auth,
    ) -> (StatusCode, Value, String) {
        let request = Request::builder().method(method).uri(uri);
        let request = match auth {
            Auth::Cookie(cookie) => request.header("cookie", cookie),
            Auth::Bearer(token) => request.header("authorization", format!("Bearer {token}")),
        };
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let raw = String::from_utf8_lossy(&bytes).to_string();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value, raw)
    }

    /// Issue #397: an **empty** manifest allowlist means "defer to the backend"
    /// — allow everything — so the status must report open mode and hand the
    /// console a non-empty starting set. The old console gate keyed off
    /// `toolkits.length > 0` and therefore rendered nothing in exactly the case
    /// where everything is permitted.
    #[tokio::test]
    async fn an_empty_toolkit_list_is_open_mode_and_still_offers_providers() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        // No `[tools.composio]` section at all — the shape 19 of 20 templates ship.
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n",
        )
        .await;

        let (status, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["granted"], true);
        assert_eq!(dto["toolkits"], json!([]), "the manifest list is verbatim");
        assert_eq!(dto["openMode"], true);
        let offered = dto["effectiveToolkits"]
            .as_array()
            .expect("open mode carries an effective toolkit list");
        assert!(
            !offered.is_empty(),
            "open mode means allow-everything, so the console must be offered providers"
        );
        assert!(
            offered.iter().any(|t| t == "github"),
            "the fallback set covers the common providers: {offered:?}"
        );
        // No credential and (in the default build) no client, so the catalog
        // cannot be read. The list is still offered — but it says so.
        assert_eq!(
            dto["catalogSource"], "fallback",
            "an unfetchable catalog must never be reported as the backend's: {dto}"
        );
        assert!(
            dto["catalogNotice"]
                .as_str()
                .is_some_and(|n| n.contains("incomplete")),
            "a fallback tells the operator it may be incomplete: {dto}"
        );
    }

    /// The company registered under `company` in `state`.
    fn runtime_of(state: &AppState, company: &str) -> std::sync::Arc<super::CompanyRuntime> {
        state
            .registry()
            .get(&CompanyId::new(company))
            .expect("company is registered")
    }

    /// A hundred-provider catalog, the shape the backend actually returns —
    /// each entry carrying the display metadata #600 stopped discarding.
    fn hundred_entries() -> Vec<CatalogEntry> {
        (0..100)
            .map(|i| CatalogEntry {
                slug: format!("provider{i:03}"),
                name: format!("Provider {i:03}"),
                description: format!("Does provider-{i:03} things."),
                logo: Some(format!("https://logos.example.test/provider{i:03}")),
                categories: vec!["productivity".to_string()],
            })
            .collect()
    }

    /// Just the slugs of [`hundred_entries`], for asserting on the slug list
    /// the wire has always carried.
    fn hundred_slugs() -> Vec<String> {
        hundred_entries().into_iter().map(|e| e.slug).collect()
    }

    /// Serialises this test against `an_admin_is_unaffected`'s env mutation.
    ///
    /// In composio builds that test repoints `COMPOSIO_BACKEND_URL_ENV` at a
    /// loopback backend for its whole body. The cache-seeding tests below derive
    /// a cache key that embeds that URL, seed the cache under it, then re-derive
    /// the key on the request path — a process-wide override landing between the
    /// two reads would change the key and strand the seeded entry, failing the
    /// `catalogSource == "backend"` assertion. `EnvVarGuard` serialises guard
    /// users against each other, so taking it here closes the race the way the
    /// crate documents (unguarded `std::env::var` readers are otherwise fair
    /// game). Gated to composio builds, where the mutation exists.
    #[cfg(feature = "composio")]
    fn composio_backend_env_guard() -> crate::test_support::EnvVarGuard {
        crate::test_support::EnvVarGuard::capture(&[
            crate::company::composio::COMPOSIO_BACKEND_URL_ENV,
        ])
    }

    /// The heart of the reopened issue: in open mode the console is offered the
    /// **backend's** catalog, not a list maintained by hand in this repo.
    ///
    /// The cache is seeded rather than a backend stood up, because what is under
    /// test is the decision — "open mode serves the fetched catalog" — and not
    /// the HTTP call, which has its own test beside
    /// `list_catalog_toolkits`. Seeding proves the thing that regressed: that a
    /// fetched answer actually reaches the response instead of being discarded
    /// in favour of the constant.
    #[tokio::test]
    async fn open_mode_serves_the_fetched_catalog_rather_than_a_hardcoded_list() {
        #[cfg(feature = "composio")]
        let _env = composio_backend_env_guard();
        let home_dir = home();
        let state = state_with_manifest_id(
            home_dir.path(),
            "catalogco",
            "[company]\nname = \"Catalog Co\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n",
        )
        .await;
        let catalog = hundred_entries();
        composio_toolkits::cache().store(
            &super::catalog_cache_key(runtime_of(&state, "catalogco").as_ref()),
            Ok(catalog.clone()),
            std::time::Instant::now(),
        );

        let (status, dto, raw) =
            send_for(&state, "catalogco", "GET", "/api/v1/company/composio", None).await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(dto["openMode"], true);
        assert_eq!(
            dto["catalogSource"], "backend",
            "a real catalog is reported as the backend's answer: {dto}"
        );
        assert_eq!(
            dto["catalogNotice"],
            Value::Null,
            "nothing to apologise for"
        );
        assert_eq!(
            dto["effectiveToolkits"],
            json!(hundred_slugs()),
            "open mode must serve what the backend permits, verbatim"
        );
        assert!(
            dto["effectiveToolkits"].as_array().unwrap().len()
                > composio_toolkits::FALLBACK_TOOLKITS.len(),
            "the fetched catalog is far longer than the built-in list — serving the \
             constant here is the exact regression this test exists to catch: {dto}"
        );
    }

    /// The important one. A company that deliberately narrowed its belt must
    /// **not** be silently widened by the catalog.
    ///
    /// The same hundred-slug catalog is in the cache; this company asked for
    /// `gmail` and gets `gmail`. Anything that unioned, defaulted-to, or fell
    /// back to the catalog for an explicit manifest would hand the company
    /// ninety-nine providers it decided against.
    #[tokio::test]
    async fn an_explicit_allowlist_is_never_widened_by_the_catalog() {
        #[cfg(feature = "composio")]
        let _env = composio_backend_env_guard();
        let home_dir = home();
        let state = state_with_manifest_id(
            home_dir.path(),
            "narrowco",
            "[company]\nname = \"Narrow Co\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n",
        )
        .await;
        composio_toolkits::cache().store(
            &super::catalog_cache_key(runtime_of(&state, "narrowco").as_ref()),
            Ok(hundred_entries()),
            std::time::Instant::now(),
        );

        let (status, dto, raw) =
            send_for(&state, "narrowco", "GET", "/api/v1/company/composio", None).await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(dto["openMode"], false);
        assert_eq!(dto["effectiveToolkits"], json!(["gmail"]));
        assert_eq!(dto["toolkits"], json!(["gmail"]));
        assert_eq!(
            dto["catalogSource"], "manifest",
            "the company's own list is the source, and the catalog is not consulted: {dto}"
        );
    }

    /// A catalog that cannot be fetched degrades to the built-in list — and the
    /// response says so, both in `catalogSource` and in words the console can
    /// show the operator. Silently serving eight slugs that look like the whole
    /// catalog is the failure this pins shut.
    #[tokio::test]
    async fn an_unfetchable_catalog_is_marked_degraded_not_passed_off_as_real() {
        #[cfg(feature = "composio")]
        let _env = composio_backend_env_guard();
        let home_dir = home();
        let state = state_with_manifest_id(
            home_dir.path(),
            "degradedco",
            "[company]\nname = \"Degraded Co\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n",
        )
        .await;
        composio_toolkits::cache().store(
            &super::catalog_cache_key(runtime_of(&state, "degradedco").as_ref()),
            Err("connection refused".to_string()),
            std::time::Instant::now(),
        );

        let (status, dto, raw) = send_for(
            &state,
            "degradedco",
            "GET",
            "/api/v1/company/composio",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(dto["openMode"], true);
        assert_eq!(
            dto["catalogSource"], "fallback",
            "the built-in list must never claim to be the backend's: {dto}"
        );
        let notice = dto["catalogNotice"]
            .as_str()
            .expect("a degraded list must explain itself");
        assert!(notice.contains("connection refused"), "{notice}");
        assert!(notice.contains("may be incomplete"), "{notice}");
        // Still usable: the operator gets something to click, just not a claim.
        assert_eq!(
            dto["effectiveToolkits"],
            json!(composio_toolkits::FALLBACK_TOOLKITS)
        );
    }

    /// A credential change drops the cached catalog: a rotated BYO token can
    /// resolve to a different Composio account, and serving the previous
    /// account's list for the rest of the TTL is the stale-by-construction
    /// answer this issue is about.
    ///
    /// Driven through a *clear* rather than a set so the status re-read at the
    /// end of the write has no credential to dial with — the assertion is about
    /// the eviction, and a test that reached the network to make it would be a
    /// different test.
    #[tokio::test]
    async fn a_credential_change_evicts_the_cached_catalog() {
        #[cfg(feature = "composio")]
        let _env = composio_backend_env_guard();
        let home_dir = home();
        let state = state_with_manifest_id(
            home_dir.path(),
            "rotateco",
            "[company]\nname = \"Rotate Co\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n",
        )
        .await;
        let key = super::catalog_cache_key(runtime_of(&state, "rotateco").as_ref());
        composio_toolkits::cache().store(&key, Ok(hundred_entries()), std::time::Instant::now());

        let (status, resp, raw) = send_for(
            &state,
            "rotateco",
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": "" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_ne!(
            resp["status"]["catalogSource"], "backend",
            "the pre-change catalog must not survive the change: {resp}"
        );
    }

    /// The other half of #397: a company that deliberately narrowed its belt
    /// sees exactly what it chose, and is NOT in open mode. A fix that offered
    /// the curated set to everyone would silently widen every restrictive
    /// manifest.
    #[tokio::test]
    async fn an_explicit_toolkit_list_is_offered_verbatim_and_is_not_open_mode() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n",
        )
        .await;

        let (status, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["openMode"], false);
        assert_eq!(dto["effectiveToolkits"], json!(["gmail"]));
        assert_eq!(dto["toolkits"], json!(["gmail"]));
    }

    /// The `credentialSource` matrix as the console consumes it, plus the
    /// write-only contract. Runs with no platform identity in the environment, so
    /// the instance contributes nothing and the company's own token is the only
    /// credential in play: `none` → `static` → `none`.
    #[tokio::test]
    async fn credential_source_tracks_the_byo_token_and_never_carries_it() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\", \"github\"]\n",
        )
        .await;

        // Initial status: granted, toolkits surfaced, no credential obtainable.
        let (status, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dto["granted"], true);
        assert_eq!(dto["credentialSource"], "none");
        assert_eq!(dto["toolkits"], json!(["gmail", "github"]));
        assert!(dto.get("backendUrl").is_some());
        assert!(
            dto.get("token").is_none(),
            "status must never carry a token"
        );
        assert!(
            dto.get("tokenConfigured").is_none(),
            "the boolean surface is replaced by credentialSource"
        );

        // Set the write-only BYO token → the static tier.
        let (status, resp, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": TOKEN })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["credentialSource"], "static");
        assert!(!raw.contains(TOKEN), "PUT response leaked the token: {raw}");

        // GET reflects it and still never carries the token.
        let (_, dto, raw) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["credentialSource"], "static");
        assert!(!raw.contains(TOKEN), "GET status leaked the token: {raw}");

        // Clearing with "" reverts to whatever the instance offers — here nothing.
        let (_, resp, _) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": "" })),
        )
        .await;
        assert_eq!(resp["status"]["credentialSource"], "none");
    }

    /// BYOK end to end over the route: storing a Composio API key moves the
    /// company onto its own account, the read plane says so without ever
    /// carrying the key, and clearing it hands the managed route back.
    #[tokio::test]
    async fn a_company_can_bring_its_own_composio_account_and_give_it_back() {
        const API_KEY: &str = "ak_not_a_real_key_0123456789";
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;
        // This test is about storing and clearing, not about the check. Pin the
        // probe clean so a `composio` build runs it against the override rather
        // than against `backend.composio.dev`.
        super::probe_override::set("acme", Ok(()));

        // Managed is the default and needs nothing stored.
        let (_, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["mode"], "managed");

        let (status, resp, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": API_KEY })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["mode"], "byok");
        assert_eq!(
            resp["status"]["credentialSource"], "static",
            "the key is this company's own, which is the static tier"
        );
        assert!(
            !raw.contains(API_KEY),
            "the PUT response leaked the key: {raw}"
        );

        // The read plane reports the route and the host it reaches, and still
        // carries no credential.
        let (_, dto, raw) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["mode"], "byok");
        assert_eq!(
            dto["backendUrl"],
            crate::company::composio::DIRECT_BASE_URL,
            "a status still naming the managed backend would read as though nothing changed"
        );
        assert!(
            !raw.contains(API_KEY),
            "the GET status leaked the key: {raw}"
        );

        // Clearing it returns the company to the managed route.
        let (_, resp, _) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": "" })),
        )
        .await;
        assert_eq!(resp["status"]["mode"], "managed");
        assert_ne!(
            resp["status"]["backendUrl"],
            crate::company::composio::DIRECT_BASE_URL
        );
    }

    // ── The managed tier, reported independently of the mode (#2275) ────

    /// The field's whole reason to exist: under BYOK, `credentialSource` names
    /// the Composio key the agents present, and `managedCredentialSource` still
    /// names the tier the **managed** route would answer with.
    ///
    /// Driven over the real route rather than over the resolver, because what
    /// regressed before is not the chain — it is whether the chain's answer
    /// survives the short-circuit `resolve_access` takes under BYOK and reaches
    /// the DTO at all.
    #[tokio::test]
    async fn the_managed_tier_is_reported_while_byok_is_selected() {
        const BYOK_KEY: &str = "ak_not_a_real_key_0123456789";
        use crate::company::company_key;
        use crate::company::composio::store_token;

        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "managedtier", GRANTED).await;
        // The check is not what this test is about; pin it clean so a
        // `composio` build answers from the override instead of the network.
        super::probe_override::set("managedtier", Ok(()));
        let runtime = runtime_of(&state, "managedtier");
        let secrets = runtime.secrets();

        async fn status(state: &AppState) -> (Value, String) {
            let (code, dto, raw) = send_for(
                state,
                "managedtier",
                "GET",
                "/api/v1/company/composio",
                None,
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{raw}");
            (dto, raw)
        }

        // Nothing stored, and (in the test process) no instance identity: both
        // fields agree, and agree on nothing.
        let (dto, _) = status(&state).await;
        assert_eq!(dto["credentialSource"], "none");
        assert_eq!(dto["managedCredentialSource"], "none");

        // A pasted `composio/token` is the managed chain's first tier.
        store_token(runtime.id(), secrets.as_ref(), "byo-managed-bearer")
            .await
            .unwrap();
        let (dto, _) = status(&state).await;
        assert_eq!(dto["credentialSource"], "static");
        assert_eq!(
            dto["managedCredentialSource"], "static",
            "under managed the two fields come from the same resolver and cannot differ: {dto}"
        );

        // Withdraw it over the company's own TinyHumans key: the chain falls
        // exactly one tier, and both fields follow it.
        store_token(runtime.id(), secrets.as_ref(), "")
            .await
            .unwrap();
        company_key::store_key(runtime.id(), secrets.as_ref(), "th_company")
            .await
            .unwrap();
        let (dto, _) = status(&state).await;
        assert_eq!(dto["credentialSource"], "company");
        assert_eq!(dto["managedCredentialSource"], "company");

        // The point. BYOK is selected, so the agents present the Composio key —
        // `static`. The managed route is untouched underneath, and the console
        // can now say so without making the operator clear the key to find out.
        let (code, resp, raw) = send_for(
            &state,
            "managedtier",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": BYOK_KEY })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["mode"], "byok");
        assert_eq!(resp["status"]["credentialSource"], "static");
        assert_eq!(
            resp["status"]["managedCredentialSource"], "company",
            "BYOK must not be able to hide which tier the managed route would answer with: {resp}"
        );
        assert!(!raw.contains(BYOK_KEY), "the PUT leaked the key: {raw}");

        let (dto, raw) = status(&state).await;
        assert_eq!(dto["credentialSource"], "static");
        assert_eq!(dto["managedCredentialSource"], "company");
        assert!(!raw.contains(BYOK_KEY), "the GET leaked the key: {raw}");
        assert!(
            dto.get("token").is_none(),
            "status must never carry a token"
        );
        assert!(
            dto.get("tokenConfigured").is_none(),
            "the boolean surface stays gone (#886): {dto}"
        );
    }

    /// The `attested` row of the matrix, which the route cannot reach without
    /// mutating the process environment.
    ///
    /// Drives the **exact pair of calls** `effective_status` makes — `access_for`
    /// for the effective tier, `resolve_credential` for the managed one — against
    /// a company already switched to BYOK, through the env seam. A company whose
    /// only credential is the instance identity presents its own Composio key and
    /// still reports `attested` as what managed would fall back to.
    #[tokio::test]
    async fn the_managed_tier_reads_the_instance_identity_under_byok() {
        use crate::app::config::MapEnv;
        use crate::company::composio::{resolve_credential, store_api_key};

        let dir = tempfile::Builder::new()
            .prefix("oc-managed-tier-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "projected-instance-token").unwrap();
        let projected = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.display().to_string(),
        )]);
        let source = super::TinyhumansTokenSource::from_env(&projected).map(std::sync::Arc::new);

        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "attestedbyok", GRANTED).await;
        let runtime = runtime_of(&state, "attestedbyok");

        // Managed, with only the instance identity: both answers are `attested`.
        assert_eq!(
            access_for(&runtime, source.clone()).await.unwrap().1,
            CredentialSource::Attested
        );
        assert_eq!(
            resolve_credential(runtime.id(), runtime.secrets().as_ref(), source.clone())
                .await
                .unwrap()
                .source(),
            CredentialSource::Attested
        );

        // Switch to BYOK. The effective tier becomes the company's own Composio
        // key; the managed chain is unchanged and still answers `attested`.
        store_api_key(
            runtime.id(),
            runtime.secrets().as_ref(),
            "ak_not_a_real_key_0123456789",
        )
        .await
        .unwrap();
        let (mode, effective) = access_for(&runtime, source.clone()).await.unwrap();
        assert_eq!(mode, super::ComposioMode::Byok);
        assert_eq!(effective, CredentialSource::Static);
        assert_eq!(
            resolve_credential(runtime.id(), runtime.secrets().as_ref(), source)
                .await
                .unwrap()
                .source(),
            CredentialSource::Attested,
            "selecting BYOK must not change what the managed route would resolve to"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── The draft-key check on `PUT …/composio/api-key` (#2275) ──────────

    /// A key Composio rejects is **not stored**, and the refusal says so in the
    /// classifier's own words.
    ///
    /// There is no rollback here because there is no write: the probe runs on
    /// the draft, before the store, which is the deliberate departure from the
    /// inference connect flow (that one writes first because its probe resolves
    /// the key by slug). The proof is the GET afterwards — the company is still
    /// on the managed route.
    #[tokio::test]
    async fn a_rejected_key_is_refused_and_nothing_is_stored() {
        const BYOK_KEY: &str = "ak_not_a_real_key_0123456789";
        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "probeauth", GRANTED).await;
        super::probe_override::set(
            "probeauth",
            Err("Composio answered 401 Unauthorized".to_string()),
        );

        let (code, body, raw) = send_for(
            &state,
            "probeauth",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": BYOK_KEY })),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(body["code"], "invalid_request", "{body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(
                    |message| message.contains(crate::company::composio_probe::describe(
                        crate::company::composio_probe::ComposioProbeClass::Auth
                    ))
                ),
            "the refusal is the classifier's copy, not an interpolated upstream string: {body}"
        );
        assert!(
            !body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("Unauthorized"),
            "the upstream reason belongs in the debug log, not in the refusal: {body}"
        );
        assert!(
            !raw.contains(BYOK_KEY),
            "a refusal must not echo the key back: {raw}"
        );

        let (_, dto, raw) =
            send_for(&state, "probeauth", "GET", "/api/v1/company/composio", None).await;
        assert_eq!(
            dto["mode"], "managed",
            "a refused key must leave the company exactly where it was: {dto}"
        );
        assert!(!raw.contains(BYOK_KEY), "the GET leaked the key: {raw}");
    }

    /// A check that fails for any reason **other** than the credential keeps the
    /// key: it is plausibly fine and only the connection is in question. The
    /// write lands, and the response carries the class and the advisory beside
    /// it.
    ///
    /// The case in the fixture is the one the ordering rule exists for — a
    /// gateway in the path — which must never read as a bad key.
    #[tokio::test]
    async fn a_key_that_could_not_be_checked_is_stored_with_an_advisory() {
        const BYOK_KEY: &str = "ak_not_a_real_key_0123456789";
        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "probeadvise", GRANTED).await;
        super::probe_override::set("probeadvise", Err("502 Bad Gateway".to_string()));

        let (code, resp, raw) = send_for(
            &state,
            "probeadvise",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": BYOK_KEY })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["mode"], "byok", "the key was stored: {resp}");
        assert_eq!(resp["probeClass"], "unknown", "{resp}");
        assert_eq!(
            resp["advisory"],
            crate::company::composio_probe::describe(
                crate::company::composio_probe::ComposioProbeClass::Unknown
            )
        );
        assert!(
            !raw.contains("Bad Gateway"),
            "the upstream text belongs in the debug log, not in operator copy: {raw}"
        );
        assert!(!raw.contains(BYOK_KEY), "the PUT leaked the key: {raw}");
    }

    /// A clean check stores the key and says nothing extra — the two additive
    /// fields are **omitted**, not null, so a consumer that predates them reads
    /// the body it always did.
    #[tokio::test]
    async fn a_clean_check_stores_the_key_and_adds_nothing_to_the_response() {
        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "probeclean", GRANTED).await;
        super::probe_override::set("probeclean", Ok(()));

        let (code, resp, raw) = send_for(
            &state,
            "probeclean",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": "ak_not_a_real_key_0123456789" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["mode"], "byok");
        assert!(resp.get("advisory").is_none(), "{resp}");
        assert!(resp.get("probeClass").is_none(), "{resp}");
    }

    /// `skipVerify` is the "add anyway" escape: a key whose check the route
    /// would have refused is stored, unexamined, when the caller says so.
    ///
    /// Defaulting matters as much as the behaviour — every test above sends no
    /// `skipVerify` at all and gets the checked path, which is what pins the
    /// `#[serde(default)]` false.
    #[tokio::test]
    async fn skip_verify_stores_a_key_the_check_would_have_refused() {
        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "probeskip", GRANTED).await;
        super::probe_override::set(
            "probeskip",
            Err("Composio answered 401 Unauthorized".to_string()),
        );

        let (code, resp, raw) = send_for(
            &state,
            "probeskip",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": "ak_not_a_real_key_0123456789", "skipVerify": true })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["mode"], "byok", "{resp}");
        assert!(
            resp.get("advisory").is_none(),
            "nothing was checked, so there is nothing to advise about: {resp}"
        );
    }

    /// Clearing is **never** checked. Withdrawing a credential is always
    /// allowed, and a probe that could refuse a clear would strand a company on
    /// a key it had already decided against.
    #[tokio::test]
    async fn clearing_a_key_is_never_checked() {
        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "probeclear", GRANTED).await;
        super::probe_override::set("probeclear", Ok(()));
        let (code, _, raw) = send_for(
            &state,
            "probeclear",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": "ak_not_a_real_key_0123456789" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");

        // Now make any probe destructive. The clear must still go through.
        super::probe_override::set(
            "probeclear",
            Err("Composio answered 401 Unauthorized".to_string()),
        );
        let (code, resp, raw) = send_for(
            &state,
            "probeclear",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": "" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["mode"], "managed", "{resp}");
        assert!(resp.get("advisory").is_none(), "{resp}");
    }

    // ── `POST …/composio/api-key/test` — check, never change ─────────────

    /// A company on the managed route has no key of its own to check, and a
    /// company in BYOK with a blank slot has none either. Both answer the same
    /// permanent `not_configured`, which is what lets the console disable the
    /// control instead of offering a check that can only fail.
    #[tokio::test]
    async fn testing_a_key_that_is_not_there_says_so_rather_than_probing() {
        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "testnokey", GRANTED).await;
        // Destructive if it ever ran. It must not run.
        super::probe_override::set(
            "testnokey",
            Err("Composio answered 401 Unauthorized".to_string()),
        );

        // Managed — the default, nothing stored.
        let (code, body, raw) = send_for(
            &state,
            "testnokey",
            "POST",
            "/api/v1/company/composio/api-key/test",
            None,
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{raw}");
        assert_eq!(body["code"], "not_configured", "{body}");

        // BYOK selected with an empty slot — the same answer, because the same
        // thing is true: there is no key here to check.
        let runtime = runtime_of(&state, "testnokey");
        runtime
            .secrets()
            .set(
                runtime.id(),
                crate::company::composio::MODE_KEY,
                crate::ports::types::SecretValue(crate::company::composio::BYOK_MODE.to_string()),
            )
            .await
            .unwrap();
        let (code, body, raw) = send_for(
            &state,
            "testnokey",
            "POST",
            "/api/v1/company/composio/api-key/test",
            None,
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{raw}");
        assert_eq!(body["code"], "not_configured", "{body}");
    }

    /// The assertion the route exists to keep honest: a **failed** check —
    /// including the destructive class — leaves the stored key and the stored
    /// mode byte-identical. Testing a credential and withdrawing it are
    /// separate acts, and nothing on this path may conflate them.
    #[tokio::test]
    async fn a_failed_check_changes_absolutely_nothing() {
        const BYOK_KEY: &str = "ak_not_a_real_key_0123456789";
        use crate::company::composio::{API_KEY_KEY, MODE_KEY};

        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "testkeeps", GRANTED).await;
        super::probe_override::set("testkeeps", Ok(()));
        let (code, _, raw) = send_for(
            &state,
            "testkeeps",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": BYOK_KEY })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");

        let runtime = runtime_of(&state, "testkeeps");
        async fn slots(runtime: &super::CompanyRuntime) -> (Option<String>, Option<String>) {
            let read = |key: &'static str| async move {
                runtime
                    .secrets()
                    .get(runtime.id(), key)
                    .await
                    .unwrap()
                    .map(|crate::ports::types::SecretValue(v)| v)
            };
            (read(MODE_KEY).await, read(API_KEY_KEY).await)
        }
        let before = slots(&runtime).await;

        // Now make the check fail in the ONE class that is destructive on the
        // write route, and check again.
        super::probe_override::set(
            "testkeeps",
            Err("Composio answered 401 Unauthorized".to_string()),
        );
        let (code, body, raw) = send_for(
            &state,
            "testkeeps",
            "POST",
            "/api/v1/company/composio/api-key/test",
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");
        assert_eq!(body["ok"], false, "{body}");
        assert_eq!(body["probeClass"], "auth", "{body}");
        assert_eq!(
            body["message"],
            crate::company::composio_probe::describe_verdict(
                crate::company::composio_probe::ComposioProbeClass::Auth
            ),
            "a check that stored nothing must not report that it saved: {body}"
        );
        assert!(
            !raw.contains(BYOK_KEY),
            "the check leaked the stored key: {raw}"
        );

        assert_eq!(
            slots(&runtime).await,
            before,
            "a failed check must leave the stored mode and key byte-identical"
        );

        // And the company is still on its own account, as the status says.
        let (_, dto, raw) =
            send_for(&state, "testkeeps", "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["mode"], "byok", "{dto}");
        assert!(!raw.contains(BYOK_KEY), "the GET leaked the key: {raw}");
    }

    /// A key Composio accepts answers `ok` and nothing else — no class, no
    /// sentence, and no credential.
    #[tokio::test]
    async fn a_clean_check_answers_ok_and_carries_no_credential() {
        const BYOK_KEY: &str = "ak_not_a_real_key_0123456789";
        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "testclean", GRANTED).await;
        super::probe_override::set("testclean", Ok(()));
        let (code, _, raw) = send_for(
            &state,
            "testclean",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": BYOK_KEY })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");

        let (code, body, raw) = send_for(
            &state,
            "testclean",
            "POST",
            "/api/v1/company/composio/api-key/test",
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");
        assert_eq!(body["ok"], true, "{body}");
        assert!(body.get("probeClass").is_none(), "{body}");
        assert!(body.get("message").is_none(), "{body}");
        assert!(!raw.contains(BYOK_KEY), "the check leaked the key: {raw}");
    }

    /// Spending the company's Composio credential against a third party is a
    /// decision taken on the company's behalf, so a member cannot trigger it —
    /// the same boundary the writes on this surface carry (issue #403).
    #[tokio::test]
    async fn a_member_cannot_test_the_company_s_composio_key() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;
        let member = crate::server::test_support::seed_session(
            &state,
            "acme",
            crate::ports::UserRole::Member,
        )
        .await;
        let (code, body, raw) = send_as(
            &state,
            "POST",
            "/api/v1/company/composio/api-key/test",
            None,
            Auth::Cookie(member),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{raw}");
        assert_eq!(body["code"], "forbidden", "{body}");
    }

    /// A build with no Composio client cannot check a key — and treats that as
    /// **un-probeable**, not as a rejected credential.
    ///
    /// No override: this drives the real `probe_transport`, which in this build
    /// is the honest "not compiled in" error. Refusing the write here would make
    /// BYOK unconfigurable on the default build; classifying an absent client as
    /// `auth` would throw away a key nothing ever looked at.
    #[cfg(not(feature = "composio"))]
    #[tokio::test]
    async fn a_build_without_the_composio_client_stores_the_key_and_says_it_could_not_check() {
        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "probenobuild", GRANTED).await;

        let (code, resp, raw) = send_for(
            &state,
            "probenobuild",
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": "ak_not_a_real_key_0123456789" })),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["mode"], "byok", "{resp}");
        assert_eq!(resp["probeClass"], "unknown", "{resp}");
    }

    /// Whose Composio account a company acts through — and therefore who pays
    /// for the calls — is an admin's decision, exactly as the backend token is.
    #[tokio::test]
    async fn a_member_cannot_bring_its_own_composio_account() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;
        let member = crate::server::test_support::seed_session(
            &state,
            "acme",
            crate::ports::UserRole::Member,
        )
        .await;

        let (status, body, raw) = send_as(
            &state,
            "PUT",
            "/api/v1/company/composio/api-key",
            Some(json!({ "apiKey": "ak_live" })),
            Auth::Cookie(member),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");
        assert_eq!(body["code"], "forbidden", "{body}");

        // The refusal is real: the route did not move.
        let (_, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(
            dto["mode"], "managed",
            "a refused write stored nothing: {dto}"
        );
    }

    /// A managed-route token stored by a company that is **on BYOK** must not
    /// claim agents have started using it.
    ///
    /// The state is newly reachable: a BYOK company whose managed chain resolves
    /// to nothing cannot be offered "Use this" — that would be switching into an
    /// outage — so the console offers the token first and the switch second, and
    /// that is the only order that works. `SWITCH_NOTE` at the end of the first
    /// step would report the second as already done, while `resolve_access` is
    /// still reading the BYOK key.
    #[tokio::test]
    async fn a_token_for_the_route_a_company_is_not_on_does_not_claim_effect() {
        use crate::company::composio::store_api_key;

        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "inactivetoken", GRANTED).await;
        let runtime = runtime_of(&state, "inactivetoken");
        // Straight to the store rather than through the route: the route probes
        // the draft key, and this test is about the note, not the probe.
        store_api_key(
            runtime.id(),
            runtime.secrets().as_ref(),
            "ak_not_a_real_key_0123456789",
        )
        .await
        .unwrap();

        let (_, resp, _) = send_for(
            &state,
            "inactivetoken",
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": TOKEN })),
        )
        .await;
        let note = resp["note"].as_str().expect("a note").to_string();
        assert!(
            !note.contains("next turn"),
            "the company is on BYOK, so agents do not pick this up next turn: {note}"
        );
        assert!(
            note.contains("still on its own Composio account"),
            "the note says why the token is not in effect: {note}"
        );
    }

    /// The note on a write names the operation (issue #1471): a set tells the
    /// operator a new token is live, a clear must not claim one exists — the
    /// effective credential after a clear is whatever tier remains, which may
    /// be nothing at all.
    #[tokio::test]
    async fn the_clear_note_does_not_claim_a_new_token() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;

        let (_, resp, _) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": TOKEN })),
        )
        .await;
        let set_note = resp["note"].as_str().expect("a note").to_string();
        assert!(
            set_note.contains("new Composio token"),
            "a set announces the new token: {set_note}"
        );

        let (_, resp, _) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": "" })),
        )
        .await;
        let clear_note = resp["note"].as_str().expect("a note").to_string();
        assert_ne!(
            clear_note, set_note,
            "set and clear are told apart: {clear_note}"
        );
        assert!(
            !clear_note.contains("new Composio token"),
            "a clear must not invent a token that no longer exists: {clear_note}"
        );
        assert!(
            clear_note.contains("cleared"),
            "the clear names what it did: {clear_note}"
        );
    }

    /// The credential tier alone.
    ///
    /// The matrix below is about credential *precedence*; which route a company
    /// takes is a separate question, asserted separately. Projecting here keeps
    /// each test about one of them.
    async fn credential_source_for(
        runtime: &CompanyRuntime,
        token_source: Option<std::sync::Arc<TinyhumansTokenSource>>,
    ) -> Result<CredentialSource, ApiError> {
        Ok(access_for(runtime, token_source).await?.1)
    }

    /// The hosted shape, driven through the env seam (no process mutation): a
    /// company that pasted nothing reads `attested` from the instance identity,
    /// its own TinyHumans key outranks that, its own Composio token outranks
    /// both, and with none of the three the answer is `none`.
    ///
    /// Drives the **real** resolver through a real secret store rather than a
    /// restatement of its precedence. A pure function that merely mirrored the
    /// rule would keep passing after the resolver lost a tier — which is exactly
    /// what the negative control for issue #586 caught.
    #[tokio::test]
    async fn credential_source_matrix_follows_the_resolver_precedence() {
        use crate::app::config::MapEnv;
        use crate::company::company_key;
        use crate::company::composio::store_token;

        let dir = tempfile::Builder::new()
            .prefix("oc-dto-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "projected-instance-token").unwrap();
        let projected = MapEnv::new([(
            crate::company::credentials::TOKEN_FILE_ENV,
            path.display().to_string(),
        )]);

        /// The instance identity a given environment resolves to.
        fn source_of(
            env: &dyn crate::app::config::EnvSource,
        ) -> Option<std::sync::Arc<super::TinyhumansTokenSource>> {
            super::TinyhumansTokenSource::from_env(env).map(std::sync::Arc::new)
        }

        let home_dir = home();
        let state = state_with_manifest_id(home_dir.path(), "matrix", GRANTED).await;
        let runtime = state
            .registry()
            .get(&CompanyId::new("matrix"))
            .expect("registered");
        let secrets = runtime.secrets();
        let id = runtime.id().clone();

        // Nothing stored + a projected instance identity → attested.
        assert_eq!(
            credential_source_for(&runtime, source_of(&projected))
                .await
                .unwrap(),
            CredentialSource::Attested
        );
        // Nothing stored at all → nothing obtainable, so no tools.
        assert_eq!(
            credential_source_for(&runtime, source_of(&MapEnv::default()))
                .await
                .unwrap(),
            CredentialSource::None
        );
        // A static instance key is the static tier.
        assert_eq!(
            credential_source_for(
                &runtime,
                source_of(&MapEnv::new([(
                    crate::company::credentials::API_KEY_ENV,
                    "th_static"
                )]))
            )
            .await
            .unwrap(),
            CredentialSource::Static
        );

        // The company's own TinyHumans key outranks the instance identity — a
        // company with a key set connects providers as *itself*, not as the pod
        // it happens to run in (issue #586)…
        company_key::store_key(&id, secrets.as_ref(), "th_company")
            .await
            .unwrap();
        assert_eq!(
            credential_source_for(&runtime, source_of(&projected))
                .await
                .unwrap(),
            CredentialSource::Company
        );
        // …and is the whole credential when the instance carries none, which is
        // the case this issue exists to fix.
        assert_eq!(
            credential_source_for(&runtime, source_of(&MapEnv::default()))
                .await
                .unwrap(),
            CredentialSource::Company
        );

        // The company's own Composio token outranks everything.
        store_token(&id, secrets.as_ref(), "byo-composio")
            .await
            .unwrap();
        assert_eq!(
            credential_source_for(&runtime, source_of(&projected))
                .await
                .unwrap(),
            CredentialSource::Static
        );
        assert_eq!(
            credential_source_for(&runtime, source_of(&MapEnv::default()))
                .await
                .unwrap(),
            CredentialSource::Static
        );

        // Clearing it falls back exactly one tier, not all the way to nothing.
        store_token(&id, secrets.as_ref(), "").await.unwrap();
        assert_eq!(
            credential_source_for(&runtime, source_of(&MapEnv::default()))
                .await
                .unwrap(),
            CredentialSource::Company
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The DTO's whole surface, serialized: a tier name and non-secret routing.
    /// No token, and no token-file path either — the console never needs to know
    /// where on disk the instance's identity lives.
    #[test]
    fn the_dto_carries_a_tier_name_and_nothing_secret() {
        let dto = ComposioStatusDto {
            in_build: true,
            granted: true,
            credential_source: CredentialSource::Attested,
            managed_credential_source: CredentialSource::Attested,
            mode: ComposioMode::Managed,
            backend_url: "https://api.tinyhumans.ai".to_string(),
            toolkits: vec!["gmail".to_string()],
            open_mode: false,
            effective_toolkits: vec!["gmail".to_string()],
            effective_catalog: vec![CatalogEntry::from_slug("gmail")],
            catalog_source: CatalogSource::Manifest,
            catalog_notice: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["credentialSource"], "attested");
        assert_eq!(json["managedCredentialSource"], "attested");
        assert_eq!(json["catalogSource"], "manifest");
        assert_eq!(json["mode"], "managed");
        let mut keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "backendUrl",
                "catalogNotice",
                "catalogSource",
                "credentialSource",
                "effectiveCatalog",
                "effectiveToolkits",
                "granted",
                "inBuild",
                "managedCredentialSource",
                "mode",
                "openMode",
                "toolkits",
            ],
            "the read shape must stay exactly this: {keys:?}"
        );
    }

    /// A `*` wildcard grant must NOT count as a composio grant on the status
    /// route (mirrors the harness build gate).
    #[tokio::test]
    async fn wildcard_grant_does_not_count_as_composio() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"*\"]\n",
        )
        .await;
        let (_, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(dto["granted"], false, "{dto}");
    }

    /// The OAuth sign-in plane is always wired into the route table (like the
    /// status route), so `POST …/composio/authorize` is never a 404. Without a
    /// usable Composio client it conflicts (`409`): on the default build because
    /// the feature is not compiled in; under the `composio` feature because no
    /// per-tenant token is configured yet. Either way the console gets a clear,
    /// non-404 signal rather than a missing route.
    #[tokio::test]
    async fn authorize_route_conflicts_without_build_or_token() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n",
        )
        .await;

        let (status, body, raw) = send(
            &state,
            "POST",
            "/api/v1/company/composio/authorize",
            Some(json!({ "toolkit": "gmail" })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{raw}");
        #[cfg(not(feature = "composio"))]
        assert_eq!(body["code"], "not_in_build", "{body}");
        #[cfg(feature = "composio")]
        assert_eq!(body["code"], "not_configured", "{body}");
    }

    // --- Who may change what the company connects through (issue #403) -------

    /// A company granting composio with one provider — the shape every
    /// authorization test below drives.
    const GRANTED: &str = "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n\
         [tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n";

    /// The regression this issue is about: a signed-in member who is not an
    /// admin cannot change what the company's agents connect through — neither
    /// by replacing the credential they present, nor by starting an OAuth
    /// handoff that would make their own account the company's connection.
    ///
    /// Both halves matter. The reported symptom was the connect flow, but the
    /// token route is the sharper one: it repoints the company's entire tool
    /// surface at whatever account the caller controls.
    #[tokio::test]
    async fn a_member_cannot_change_what_the_company_connects_through() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;
        let member = crate::server::test_support::seed_session(
            &state,
            "acme",
            crate::ports::UserRole::Member,
        )
        .await;

        let (status, body, raw) = send_as(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": TOKEN })),
            Auth::Cookie(member.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");
        assert_eq!(body["code"], "forbidden", "{body}");
        assert!(
            body["error"].as_str().unwrap_or_default().contains("admin"),
            "the refusal has to say why it was refused: {body}"
        );

        let (status, body, raw) = send_as(
            &state,
            "POST",
            "/api/v1/company/composio/authorize",
            Some(json!({ "toolkit": "gmail" })),
            Auth::Cookie(member.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");
        assert_eq!(body["code"], "forbidden", "{body}");

        // And the refusal is real, not merely a different status: the token
        // never landed, so the company's credential is untouched.
        let (_, dto, _) = send(&state, "GET", "/api/v1/company/composio", None).await;
        assert_eq!(
            dto["credentialSource"], "none",
            "a refused write must not have stored anything: {dto}"
        );
    }

    /// The other side of the boundary, and the reason this is a role check
    /// rather than a removal: an admin still does both things. Without the
    /// feature, `authorize` answers `409` at the build boundary; with it, this
    /// test supplies a loopback backend and proves the admin reaches the real
    /// authorization call without dialling production (issue #801).
    #[tokio::test]
    async fn an_admin_is_unaffected() {
        #[cfg(feature = "composio")]
        let backend = spawn_authorize_backend().await;
        #[cfg(feature = "composio")]
        let env = crate::test_support::EnvVarGuard::capture(&[
            crate::company::composio::COMPOSIO_BACKEND_URL_ENV,
        ]);
        #[cfg(feature = "composio")]
        env.set(crate::company::composio::COMPOSIO_BACKEND_URL_ENV, &backend);

        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;

        let (status, resp, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": TOKEN })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(resp["status"]["credentialSource"], "static");

        let (status, _body, raw) = send(
            &state,
            "POST",
            "/api/v1/company/composio/authorize",
            Some(json!({ "toolkit": "gmail" })),
        )
        .await;
        #[cfg(feature = "composio")]
        assert_eq!(
            status,
            StatusCode::OK,
            "an admin reaches authorization: {raw}"
        );
        #[cfg(feature = "composio")]
        assert_eq!(
            _body["connectUrl"], "https://composio.test/connect/gmail",
            "the handler returns the loopback backend's authorization URL: {_body}"
        );
        #[cfg(not(feature = "composio"))]
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "an admin reaches the build check, not the role check: {raw}"
        );
    }

    /// Reads stay open to any member, and this pins that decision either way.
    ///
    /// Knowing *that* Gmail is connected is what lets a member understand why
    /// an agent can read mail; it carries no credential. Only the ability to
    /// change it needed an owner. If a future change decides reads are
    /// sensitive too, this test is the thing that has to be edited on purpose.
    #[tokio::test]
    async fn a_member_may_still_read_the_composio_status_and_connections() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;
        let member = crate::server::test_support::seed_session(
            &state,
            "acme",
            crate::ports::UserRole::Member,
        )
        .await;

        let (status, dto, raw) = send_as(
            &state,
            "GET",
            "/api/v1/company/composio",
            None,
            Auth::Cookie(member.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert_eq!(dto["granted"], true);
        assert!(
            dto.get("token").is_none(),
            "a member's read must not carry a credential either: {dto}"
        );

        // 409 (no client in this build), NOT 403 — the read is not role-gated.
        let (status, _, raw) = send_as(
            &state,
            "GET",
            "/api/v1/company/composio/connections",
            None,
            Auth::Cookie(member),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{raw}");
    }

    /// A machine credential may set the company's token — and is **named** in
    /// the trail when it does.
    ///
    /// This is the deliberate half of the issue's "cannot, or if it may, the
    /// write is attributed". Refusing the hosting control plane a route it
    /// already sits above (it provisions the tenant and holds its database
    /// credentials) would be ceremony, not a boundary, and it would contradict
    /// the two-principal model in `docs/spec/runtime/config.md`. What it does
    /// not get is anonymity: the entry names the tenant, so a machine-made
    /// change is as reviewable afterwards as a human one.
    #[tokio::test]
    async fn a_machine_credential_is_named_when_it_sets_the_token() {
        use crate::server::platform_auth::{
            PlatformAuthConfig, PlatformClaims, UnsignedTenantVerifier,
        };

        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED)
            .await
            .with_platform_auth(PlatformAuthConfig::new(std::sync::Arc::new(
                UnsignedTenantVerifier::new("test-platform-secret"),
            )));
        let token = UnsignedTenantVerifier::tenant_token(&PlatformClaims {
            tenant: "tenant:platform".to_string(),
            scopes: std::collections::HashSet::from(["platform".to_string()]),
            companies: None,
        });

        let (status, _, raw) = send_as(
            &state,
            "PUT",
            "/api/v1/company/composio/token",
            Some(json!({ "token": TOKEN })),
            Auth::Bearer(token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");

        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let events = runtime
            .events()
            .read_from(runtime.id(), crate::ports::types::EventSeq::new(0), 100)
            .await
            .unwrap();
        let by = events
            .iter()
            .find_map(|stored| match &stored.event {
                crate::ports::types::CompanyEvent::ToolAccessChanged { by, .. } => by.clone(),
                _ => None,
            })
            .expect("the machine write is journaled");
        assert_eq!(
            by.kind,
            crate::ports::types::ActorKind::System,
            "a machine is a machine, not a person: {by:?}"
        );
        assert_eq!(by.id, "tenant:platform", "the tenant is named: {by:?}");
    }

    /// Every accepted change to the company's tool access names the person who
    /// made it, so "how did we come to be connected through that account" is
    /// answerable afterwards.
    ///
    /// Set and clear are journaled as distinct words — one grants access and
    /// the other withdraws it, and an audit trail that conflated them would be
    /// worth little.
    #[tokio::test]
    async fn a_credential_change_records_who_made_it() {
        use crate::ports::types::{ActorKind, CompanyEvent, EventSeq};

        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;

        for token in [TOKEN, ""] {
            let (status, _, raw) = send(
                &state,
                "PUT",
                "/api/v1/company/composio/token",
                Some(json!({ "token": token })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{raw}");
        }

        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        let events = runtime
            .events()
            .read_from(runtime.id(), EventSeq::new(0), 100)
            .await
            .unwrap();
        let changes: Vec<(String, Option<String>)> = events
            .iter()
            .filter_map(|stored| match &stored.event {
                CompanyEvent::ToolAccessChanged {
                    change,
                    toolkit,
                    by,
                } => {
                    let by = by.as_ref().expect("an admin write is always attributed");
                    assert_eq!(by.kind, ActorKind::User, "attributed to a person");
                    assert!(!by.id.is_empty(), "the person is named");
                    Some((change.clone(), toolkit.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            changes,
            vec![
                ("credential_set".to_string(), None),
                ("credential_cleared".to_string(), None),
            ],
            "a set and a clear are distinct entries in the trail"
        );

        // The trail is an audit record, not a second place a secret lives.
        let raw = serde_json::to_string(&events).unwrap();
        assert!(!raw.contains(TOKEN), "the journal leaked the token: {raw}");
    }

    /// `GET …/composio/connections` is likewise always wired and conflicts
    /// (`409`) when there is no usable client — no build feature or no token.
    #[tokio::test]
    async fn connections_route_conflicts_without_build_or_token() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n",
        )
        .await;

        let (status, body, raw) =
            send(&state, "GET", "/api/v1/company/composio/connections", None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{raw}");
        #[cfg(not(feature = "composio"))]
        assert_eq!(body["code"], "not_in_build", "{body}");
        #[cfg(feature = "composio")]
        assert_eq!(body["code"], "not_configured", "{body}");
    }

    /// The disconnect added for #404 is wired on the same terms as the rest of
    /// the OAuth plane: present in the route table whatever the build, and a
    /// `409` — never a `404` — when there is no usable client. A `404` here
    /// would read as "no such connection", which is a claim about the company's
    /// accounts that this build cannot make.
    #[tokio::test]
    async fn disconnect_route_conflicts_without_build_or_token() {
        let home_dir = home();
        let home = home_dir.path().to_path_buf();
        let state = state_with_manifest(
            &home,
            "[company]\nname = \"Acme\"\n[policy]\nmode = \"full\"\n[tools]\nallow = [\"composio\"]\n[tools.composio]\ntoolkits = [\"gmail\"]\n",
        )
        .await;

        let (status, body, raw) = send(
            &state,
            "DELETE",
            "/api/v1/company/composio/connections/conn-1",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{raw}");
        #[cfg(not(feature = "composio"))]
        assert_eq!(body["code"], "not_in_build", "{body}");
        #[cfg(feature = "composio")]
        assert_eq!(body["code"], "not_configured", "{body}");
    }

    /// The ops tests that are decidable only in a build carrying `composio`.
    ///
    /// Gathered under one module so a CI lane can *name* them. A feature-gated
    /// test's default fate in this repo is "compiled by `Check
    /// (--all-features)`, executed by nothing" (issue #770), and the composio
    /// lane has to select by filter rather than run this whole module: with the
    /// feature on, `an_admin_is_unaffected` dials `api.tinyhumans.ai` for real
    /// (issue #801). One filter on this module runs every gated test here and
    /// none of that, and a gated test added later is picked up by joining the
    /// module rather than by remembering to edit `ci.yml`.
    #[cfg(feature = "composio")]
    mod gated_tests {
        use super::*;
        use crate::server::ops::composio::{drop_dangling_defaults, group_by_toolkit};

        /// Issue #404: the per-toolkit shape the tile grid reads is a fold over the
        /// per-connection rows, and the fold must not lose an account or flip a
        /// boolean. Pure — no live backend needed.
        #[test]
        fn grouping_keeps_every_account_and_ors_their_connected_state() {
            use crate::harness::composio::ComposioConnectionRow;

            let row = |id: &str, toolkit: &str, connected: bool, account: Option<&str>| {
                ComposioConnectionRow {
                    id: id.to_string(),
                    toolkit: toolkit.to_string(),
                    status: if connected { "ACTIVE" } else { "INITIATED" }.to_string(),
                    connected,
                    created_at: None,
                    account: account.map(str::to_string),
                }
            };

            let out = group_by_toolkit(
                vec![
                    row("c1", "gmail", false, Some("a@acme.test")),
                    row("c2", "gmail", true, Some("b@acme.test")),
                    row("c3", "slack", false, None),
                ],
                &Default::default(),
            );

            assert_eq!(out.len(), 2, "one entry per toolkit");
            assert_eq!(out[0].toolkit, "gmail");
            assert!(
                out[0].connected,
                "a toolkit is connected when ANY of its accounts is — the second row \
                 here, which a first-row-wins fold would have missed"
            );
            assert_eq!(
                out[0]
                    .accounts
                    .iter()
                    .map(|a| (a.id.as_str(), a.connected))
                    .collect::<Vec<_>>(),
                vec![("c1", false), ("c2", true)],
                "both accounts survive, in the order the rows arrived"
            );
            assert_eq!(out[1].toolkit, "slack");
            assert!(!out[1].connected, "no active account, so not connected");
            assert_eq!(out[1].accounts.len(), 1);

            // Nothing pinned: nothing is marked, and no default is reported. This
            // is the shape #819 asks the console to render honestly, and it stays
            // the shape until somebody chooses.
            assert!(out.iter().all(|dto| dto.default_connection_id.is_none()));
            assert!(
                out.iter()
                    .flat_map(|dto| dto.accounts.iter())
                    .all(|account| !account.is_default),
                "an unchosen account is never marked as the default"
            );
        }

        /// Issue #820: once a company has chosen, the choice is reported on the
        /// toolkit **and** marked on the one account it names — the console needs
        /// both to draw a list with one row marked and the rest offering to become
        /// it.
        #[test]
        fn grouping_marks_the_chosen_account_and_only_that_one() {
            use crate::harness::composio::ComposioConnectionRow;

            let row = |id: &str, toolkit: &str| ComposioConnectionRow {
                id: id.to_string(),
                toolkit: toolkit.to_string(),
                status: "ACTIVE".to_string(),
                connected: true,
                created_at: None,
                account: None,
            };
            let defaults: crate::company::composio::ComposioDefaults =
                [("gmail".to_string(), "c2".to_string())]
                    .into_iter()
                    .collect();

            let out = group_by_toolkit(
                vec![row("c1", "gmail"), row("c2", "gmail"), row("c3", "slack")],
                &defaults,
            );

            assert_eq!(out[0].default_connection_id.as_deref(), Some("c2"));
            assert_eq!(
                out[0]
                    .accounts
                    .iter()
                    .map(|a| (a.id.as_str(), a.is_default))
                    .collect::<Vec<_>>(),
                vec![("c1", false), ("c2", true)],
                "exactly one account carries the mark"
            );
            assert!(
                out[1].default_connection_id.is_none(),
                "a choice made for gmail says nothing about slack: {:?}",
                out[1].default_connection_id
            );
        }

        /// Issue #820: an account revoked **at Composio** — not through this console,
        /// so nothing here saw the disconnect — leaves a choice naming a connection
        /// that no longer exists. That choice is not merely stale: it is sent on the
        /// next `composio_execute` and refused, so the toolkit stops working for
        /// every agent for a reason nothing on screen explains. The read the console
        /// polls repairs it.
        ///
        /// Driven through the handler's own helper rather than the route, because
        /// the route needs a live Composio backend and the decision under test is
        /// the one made *after* it answers. What it must not do is as load-bearing
        /// as what it must: a live choice is untouched, and a toolkit whose chosen
        /// account is gone falls back to "Composio picks" rather than being
        /// re-pointed at a sibling account nobody chose.
        #[tokio::test]
        async fn the_connections_read_forgets_a_choice_composio_no_longer_lists() {
            use crate::company::composio::{load_defaults, set_default};
            use crate::harness::composio::ComposioConnectionRow;

            let home_dir = home();
            let state = state_with_manifest(home_dir.path(), GRANTED).await;
            let runtime = runtime_of(&state, "acme");
            let (id, secrets) = (runtime.id(), runtime.secrets());

            // gmail names an account still live; slack names one revoked since.
            for (toolkit, connection) in [("gmail", "c1"), ("slack", "c_revoked")] {
                set_default(id, secrets.as_ref(), toolkit, connection)
                    .await
                    .unwrap();
            }

            let row = |id: &str, toolkit: &str| ComposioConnectionRow {
                id: id.to_string(),
                toolkit: toolkit.to_string(),
                status: "ACTIVE".to_string(),
                connected: true,
                created_at: None,
                account: None,
            };
            // The company still holds a slack account — just not the chosen one.
            let rows = vec![row("c1", "gmail"), row("c9", "slack")];

            let left = drop_dangling_defaults(
                runtime.as_ref(),
                &rows,
                load_defaults(id, secrets.as_ref()).await.unwrap(),
            )
            .await
            .expect("the cleanup completes");

            assert_eq!(
                left.get("gmail").map(String::as_str),
                Some("c1"),
                "a choice naming a live account is untouched"
            );
            assert!(
                !left.contains_key("slack"),
                "the choice naming a revoked account is dropped: {left:?}"
            );
            assert_eq!(
                load_defaults(id, secrets.as_ref()).await.unwrap(),
                left,
                "the repair is stored, not merely reflected in this one response — \
                 otherwise the next agent turn still sends the dead id"
            );

            let out = group_by_toolkit(rows, &left);
            assert_eq!(out[1].toolkit, "slack");
            assert!(
                out[1].default_connection_id.is_none()
                    && out[1].accounts.iter().all(|account| !account.is_default),
                "with its choice gone slack is unchosen again — the surviving account \
                 is not silently promoted into a decision nobody made: {:?}",
                out[1]
            );
            assert_eq!(out[0].default_connection_id.as_deref(), Some("c1"));
        }

        /// A loopback backend for the `DELETE …/composio/connections/{id}` route
        /// test: `conn-1` is the only account this company knows about, and the
        /// backend's own delete either succeeds or fails depending on
        /// `fail_delete`, so the same mock drives both the 404 and the 502 arm of
        /// the mapping in [`super::super::disconnect_impl`].
        async fn spawn_connections_backend(
            fail_delete: bool,
        ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
            use axum::extract::Path;
            use axum::response::IntoResponse;

            let deletes: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
            let recorded = deletes.clone();
            let app = Router::new()
                .route(
                    "/agent-integrations/composio/connections",
                    axum::routing::get(|| async {
                        Json(json!({
                            "success": true,
                            "data": { "connections": [
                                { "id": "conn-1", "toolkit": "gmail", "status": "ACTIVE" }
                            ] }
                        }))
                    }),
                )
                .route(
                    "/agent-integrations/composio/connections/{id}",
                    axum::routing::delete(move |Path(id): Path<String>| {
                        let recorded = recorded.clone();
                        async move {
                            recorded.lock().unwrap().push(id);
                            if fail_delete {
                                StatusCode::INTERNAL_SERVER_ERROR.into_response()
                            } else {
                                Json(json!({ "success": true, "data": { "deleted": true } }))
                                    .into_response()
                            }
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            (format!("http://{addr}"), deletes)
        }

        /// The documented split in [`super::super::disconnect_impl`]: an id this
        /// company's own read cannot see is a `404`, never a `502`, because it is a
        /// claim about the company's accounts rather than about the provider. Route
        /// tests above only reach the "no client at all" `409` arm; this drives the
        /// real mapping with a loopback backend standing in for Composio.
        #[tokio::test]
        async fn disconnect_maps_an_unknown_id_to_404_not_502() {
            let (backend, deletes) = spawn_connections_backend(false).await;
            let env = crate::test_support::EnvVarGuard::capture(&[
                crate::company::composio::COMPOSIO_BACKEND_URL_ENV,
            ]);
            env.set(crate::company::composio::COMPOSIO_BACKEND_URL_ENV, &backend);

            let home_dir = home();
            let state = state_with_manifest(home_dir.path(), GRANTED).await;
            send(
                &state,
                "PUT",
                "/api/v1/company/composio/token",
                Some(json!({ "token": TOKEN })),
            )
            .await;

            let (status, body, raw) = send(
                &state,
                "DELETE",
                "/api/v1/company/composio/connections/conn-does-not-exist",
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{raw}");
            assert_eq!(body["code"], "not_found", "{body}");
            assert!(
                deletes.lock().unwrap().is_empty(),
                "an id outside the company's visible connections must never reach a delete call"
            );
        }

        /// The other arm of the same split: an id the company DOES hold, but the
        /// backend refuses to delete, is a `502` naming the provider failure —
        /// never a `404`, which would tell the operator to stop looking for an
        /// account that is right there in the list.
        #[tokio::test]
        async fn disconnect_maps_a_backend_failure_to_502_not_404() {
            let (backend, deletes) = spawn_connections_backend(true).await;
            let env = crate::test_support::EnvVarGuard::capture(&[
                crate::company::composio::COMPOSIO_BACKEND_URL_ENV,
            ]);
            env.set(crate::company::composio::COMPOSIO_BACKEND_URL_ENV, &backend);

            let home_dir = home();
            let state = state_with_manifest(home_dir.path(), GRANTED).await;
            send(
                &state,
                "PUT",
                "/api/v1/company/composio/token",
                Some(json!({ "token": TOKEN })),
            )
            .await;

            let (status, body, raw) = send(
                &state,
                "DELETE",
                "/api/v1/company/composio/connections/conn-1",
                None,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{raw}");
            assert_eq!(body["code"], "tinyhumans_composio_disconnect", "{body}");
            assert_eq!(
                deletes.lock().unwrap().as_slice(),
                ["conn-1"],
                "a known id must actually reach the backend's delete before failing"
            );
        }
    }

    /// The choice plane is wired on the same terms as the rest of the OAuth
    /// plane: in the route table whatever the build, and a `409` — never a
    /// `404` — when there is no usable client, since "no such connection" is a
    /// claim about this company's accounts that a build without Composio cannot
    /// make.
    #[tokio::test]
    async fn set_default_route_conflicts_without_build_or_token() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;

        let (status, body, raw) = send(
            &state,
            "PUT",
            "/api/v1/company/composio/connections/conn-1/default",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{raw}");
        #[cfg(not(feature = "composio"))]
        assert_eq!(body["code"], "not_in_build", "{body}");
        #[cfg(feature = "composio")]
        assert_eq!(body["code"], "not_configured", "{body}");
    }

    /// Clearing is the deliberate exception: it takes no upstream call, so it
    /// works in a build without Composio and — the case that matters — when the
    /// provider is unreachable or the account is already gone. A clear that
    /// needed the network would refuse exactly when it is most needed.
    #[tokio::test]
    async fn clearing_a_choice_needs_no_client_and_is_idempotent() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;

        let (status, body, raw) = send(
            &state,
            "DELETE",
            "/api/v1/company/composio/connections/conn-1/default",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert!(
            body["note"]
                .as_str()
                .unwrap_or_default()
                .contains("nothing changed"),
            "clearing what was never chosen says so rather than claiming a change: {body}"
        );

        // Now with something stored, the same call reports the real change and
        // leaves nothing behind.
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        crate::company::composio::set_default(
            runtime.id(),
            runtime.secrets().as_ref(),
            "gmail",
            "conn-1",
        )
        .await
        .unwrap();

        let (status, body, raw) = send(
            &state,
            "DELETE",
            "/api/v1/company/composio/connections/conn-1/default",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{raw}");
        assert!(
            body["note"]
                .as_str()
                .unwrap_or_default()
                .contains("Cleared"),
            "{body}"
        );
        assert!(
            crate::company::composio::load_defaults(runtime.id(), runtime.secrets().as_ref())
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Choosing the account a company acts as is an admin's decision, like
    /// connecting and disconnecting one: every agent in the company acts
    /// through the single answer, so it is not a per-operator preference.
    #[tokio::test]
    async fn a_member_cannot_choose_the_account_the_company_acts_as() {
        let home_dir = home();
        let state = state_with_manifest(home_dir.path(), GRANTED).await;
        let member = crate::server::test_support::seed_session(
            &state,
            "acme",
            crate::ports::UserRole::Member,
        )
        .await;

        for method in ["PUT", "DELETE"] {
            let (status, body, raw) = send_as(
                &state,
                method,
                "/api/v1/company/composio/connections/conn-1/default",
                None,
                Auth::Cookie(member.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method}: {raw}");
            assert_eq!(body["code"], "forbidden", "{method}: {body}");
        }

        // And the refusal is real: nothing was stored by either attempt.
        let runtime = state.registry().get(&CompanyId::new("acme")).unwrap();
        assert!(
            crate::company::composio::load_defaults(runtime.id(), runtime.secrets().as_ref())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
