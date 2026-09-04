//! Hosted multi-tenant provisioning and per-company lifecycle controls.
//!
//! `POST /api/v1/companies` provisions a company from a manifest body (raw TOML
//! or `{ "manifest_toml", "id"? }` JSON), validates it, builds a
//! [`CompanyRuntime`](crate::company::runtime::CompanyRuntime) over the data
//! dir, registers it, and records its owning tenant. Provisioning and suspension
//! require the `platform` scope; archive is owner-scoped; the pause, resume and
//! emergency-stop routes additionally require authority over the company —
//! [`AdminScopedCompany`] — because they decide something for the whole company
//! rather than for the caller. None of them cross tenants.
//!
//! Lifecycle transitions persist the new [`CompanyRecord`](crate::ports::types::CompanyRecord)
//! `lifecycle` and append a [`LifecycleChanged`](crate::ports::types::CompanyEvent::LifecycleChanged)
//! audit event. Archive additionally removes the company from the registry so it
//! is no longer addressable.
//!
//! Webhook emission (`approval.requested`, `work.completed`, `feedback.created`,
//! `budget.exhausted`) runs through the offline-mockable
//! [`WebhookSink`](crate::server::webhook::WebhookSink); the default build
//! records deliveries in memory.

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::app::config::AuthMode;
use crate::company::CompanyManifest;
use crate::ports::CompanyStore;
use crate::ports::types::{Actor, ActorKind, CompanyId};
use crate::runtime::types::CycleReport;
use crate::runtime::{RuntimeBuilder, company_id_from_name};
use crate::server::error::ApiError;
use crate::server::graphql::auth::GqlAuth;
use crate::server::ops::AdminScopedCompany;
use crate::server::platform_auth::{PlatformScope, acting_tenant, authorize_address};
use crate::server::webhook::{WebhookEvent, WebhookKind};
use crate::store::FsCompanyStore;

/// Builds the provisioning + lifecycle route fragment.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/companies", post(provision))
        .route("/api/v1/companies/provisioning", get(provisioning_info))
        .route("/api/v1/companies/{id}/pause", post(pause))
        .route("/api/v1/companies/{id}/resume", post(resume))
        .route(
            "/api/v1/companies/{id}/emergency-pause",
            post(emergency_pause),
        )
        .route(
            "/api/v1/companies/{id}/emergency-resume",
            post(emergency_resume),
        )
        .route("/api/v1/companies/{id}/suspend", post(suspend))
        .route("/api/v1/companies/{id}/archive", post(archive))
}

// ---------------------------------------------------------------------------
// Response envelopes
// ---------------------------------------------------------------------------

fn envelope(status: StatusCode, code: &str, error: &str) -> Response {
    (status, Json(json!({ "error": error, "code": code }))).into_response()
}

fn not_found(id: &str) -> Response {
    envelope(
        StatusCode::NOT_FOUND,
        "company_not_found",
        &format!("company not found: {id}"),
    )
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// What `GET /api/v1/companies/provisioning` reports: the sign-in mode a
/// company provisioned on this host right now would land in, and whether that
/// mode requires the create/reset dialog to collect wallet addresses.
///
/// Reachable by a console platform bearer, unlike `GET /api/v1/setup` (loopback
/// / peer-gated), so the create/reset dialog can render mode-appropriate fields
/// before it provisions rather than discovering the host's `wallet` override
/// only when the manifest is refused (`auth_mode_wallet_no_wallets`).
#[derive(Debug, Serialize)]
struct ProvisioningInfoDto {
    /// The effective sign-in mode: `wallet`, `email`, or `none`.
    auth_mode: &'static str,
    /// Whether provisioning requires at least one `[users].wallets` address.
    wallets_required: bool,
}

/// `GET /api/v1/companies/provisioning` — the auth-mode preflight the create /
/// reset dialog reads before building a manifest.
async fn provisioning_info(
    PlatformScope(_claims): PlatformScope,
    State(state): State<AppState>,
) -> Response {
    // When no host-wide override is set, each company's own `[users].mode`
    // decides — and the console builds an `email` manifest by default, so the
    // default report is `email`. A `wallet` override is the case that forces
    // the dialog to collect addresses.
    let mode = state.auth_mode_override().unwrap_or_default();
    let dto = ProvisioningInfoDto {
        auth_mode: mode.as_str(),
        wallets_required: mode == AuthMode::Wallet,
    };
    (StatusCode::OK, Json(dto)).into_response()
}

/// The JSON provisioning body: a manifest string plus an optional explicit id.
#[derive(Debug, Deserialize)]
struct ProvisionBody {
    /// The company manifest as TOML.
    manifest_toml: String,
    /// An explicit company id; derived from the name when omitted.
    #[serde(default)]
    id: Option<String>,
}

/// Did the submitted manifest **text** name an approval tier?
///
/// Read from the raw TOML rather than from the parsed [`CompanyManifest`], and
/// that is the whole point of the function. `Policy::mode` is a plain `String`
/// behind a serde default, so a parsed manifest *always* carries a mode and can
/// never be asked what its author actually wrote. Only the text distinguishes
/// "the author chose `supervised`" from "the author said nothing".
///
/// Anything that does not parse as a table with `policy.mode` set counts as not
/// declared. Unparseable text cannot reach here — the caller has already
/// rejected it — so the `unwrap_or(false)` is for the shape, not for a case
/// this can actually meet.
fn declares_policy_mode(manifest_toml: &str) -> bool {
    toml::from_str::<toml::Value>(manifest_toml)
        .ok()
        .and_then(|value| {
            value
                .get("policy")
                .and_then(|policy| policy.get("mode"))
                .cloned()
        })
        .is_some()
}

/// `POST /api/v1/companies` — provision a company from a manifest body.
async fn provision(
    PlatformScope(claims): PlatformScope,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Accept raw TOML or a JSON envelope carrying the TOML.
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("json"))
        .unwrap_or(false);
    let (manifest_toml, explicit_id) = if is_json {
        match serde_json::from_str::<ProvisionBody>(&body) {
            Ok(parsed) => (parsed.manifest_toml, parsed.id),
            Err(err) => {
                return envelope(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    &format!("invalid provisioning body: {err}"),
                );
            }
        }
    } else {
        (body, None)
    };

    let mut manifest: CompanyManifest = match toml::from_str(&manifest_toml) {
        Ok(manifest) => manifest,
        Err(err) => {
            return envelope(
                StatusCode::BAD_REQUEST,
                "manifest_parse",
                &format!("manifest is not valid TOML: {}", err.message()),
            );
        }
    };
    // Issue #605: a company provisioned without a stated tier gets `auto`,
    // recorded explicitly rather than inherited from a serde default.
    //
    // This is the one creation path with no template behind it. The other two
    // both arrive carrying a tier already: `serve --company <dir>` and the
    // desktop app read a `companies/*/company.toml`, and every shipped preset
    // now declares `mode`.
    //
    // Written onto the manifest *before* the build, so the tier lands in the
    // stored manifest the same way an authored one would. A provisioned tenant
    // has no `company.toml` anywhere, so that record is the only place an
    // operator can read their tier — leaving it implicit would make it
    // unreadable rather than merely unstated.
    if !declares_policy_mode(&manifest_toml) {
        manifest.policy.mode = crate::company::PROVISIONED_POLICY_MODE.to_string();
    }
    let problems = manifest.validate();
    if !problems.is_empty() {
        // Render the prosumer problem list; never leak serde traces.
        return ApiError(crate::error::OpenCompanyError::ManifestInvalid {
            path: std::path::PathBuf::from("company.toml"),
            problems,
        })
        .into_response();
    }

    // The same refusal boot applies to `serve --company`: a company with no
    // sign-in on a host anyone can reach is an unauthenticated admin console,
    // not a desktop app. A tenant's manifest can request `[users].mode =
    // "none"`, but this host will not silently serve it wherever it binds —
    // it is refused here exactly as it would be refused at boot.
    //
    // Checked here, before `id` is even resolved, rather than after
    // `builder.build()` returns: `RuntimeBuilder::build` persists the
    // `CompanyRecord` as one of its last steps and returns success, so a
    // post-build refusal still leaves that record durably saved. The
    // duplicate-id check further up treats any durable record as a live
    // occupant of `id` — so a caller who fixed the manifest (switched to
    // `email` or `wallet`) and retried the exact recovery this error message
    // recommends got `company_exists` forever, for an id that was never
    // actually provisioned (issue #1828 comment 3866012835). Resolving the
    // effective auth mode from the manifest and the host override — the same
    // two inputs `RuntimeBuilder::build` combines at
    // `self.auth_mode_override.unwrap_or_else(...)` — lets this reproduce
    // that decision without paying for the build, so a rejected request never
    // reserves the id in the first place.
    //
    // Read ONCE, here, and reused for both this check and the builder below
    // (`.with_auth_mode_override(auth_mode_override)`) rather than calling
    // `state.auth_mode_override()` a second time at the builder. The override
    // lives behind `AppState`'s `RwLock` and `setup.rs` can flip it from a
    // concurrent request at any point — including during this request's own
    // `.await`s between here and the builder (the duplicate-id store lookup,
    // notably). Two independent reads could then observe two different
    // values: this check validates against the first, but a build using the
    // second could silently produce a runtime in a mode the validated
    // manifest never agreed to — e.g. an admins-only manifest, checked and
    // passed against `email`, built as `wallet` with no eligible wallet
    // users. On a reset the old company is already archived by the time that
    // divergence could happen, so the replacement would be the only copy left
    // (issue #1828 comment 3873451846).
    let auth_mode_override = state.auth_mode_override();
    let effective_auth_mode = auth_mode_override
        .unwrap_or_else(|| AuthMode::from_str(&manifest.users.mode).unwrap_or_default());
    if !effective_auth_mode.has_login() && !state.config().is_local_only() {
        return envelope(
            StatusCode::BAD_REQUEST,
            "auth_mode_none_not_allowed",
            "this manifest sets `[users].mode = \"none\"`, which has no sign-in, but this \
             host binds a routable address and would serve it to anyone who can reach it. \
             Choose `email` or `wallet`, or bind loopback.",
        );
    }
    // A `wallet`-mode host has no environment counterpart to
    // `OPENCOMPANY_ADMIN_EMAIL`: `manifest_wallets` (`server/users/wallet.rs`)
    // reads `[users].wallets` alone, with no deployment-wide bootstrap grant,
    // because that variable exists for a *platform-provisioned* company whose
    // creator the control plane knows only as an email address, never a
    // wallet. The console's create/reset dialog always emits `[users].admins`
    // — the only field it knows how to fill (`buildManifestToml`,
    // `company-manifest.ts`) — which `email` mode reads and `wallet` mode
    // never does. `manifest.validate()` above cannot catch this: it checks the
    // manifest's own declared `[users].mode` (defaulted to `email`) against
    // its own admin/wallet lists for self-consistency, and finds none here,
    // because the mismatch only exists between the manifest and the host's
    // override — which is exactly what `effective_auth_mode` (just resolved
    // above, the same way `RuntimeBuilder::build` will) makes visible.
    // Checked here, before `id` is resolved, for the same reason as the
    // `none`-mode refusal immediately above: a request refused after
    // `builder.build()` still leaves its `CompanyRecord` durably saved, so a
    // caller who fixed the manifest and retried would find the id
    // permanently reserved by a company that never actually provisioned —
    // and on a reset, the old company is archived before this point is ever
    // reached (issue #1828 comment 3866132491).
    if effective_auth_mode == AuthMode::Wallet && manifest.users.wallets.is_empty() {
        return envelope(
            StatusCode::BAD_REQUEST,
            "auth_mode_wallet_no_wallets",
            "this host signs users in with wallets, but this manifest lists no \
             `[users].wallets` — only `[users].admins`, which `wallet` mode never reads, and \
             there is no deployment-wide wallet bootstrap the way `OPENCOMPANY_ADMIN_EMAIL` \
             provides for `email` mode. Add at least one base58 wallet address to \
             `[users].wallets`, or ask the host to switch modes.",
        );
    }

    let id = match explicit_id {
        Some(raw) => CompanyId::new(raw),
        None => company_id_from_name(&manifest.company.name),
    };
    // The tenant that owns this company and namespaces its id.
    //
    // In shared-single-DB mode the workload's *configured* namespace
    // (`OPENCOMPANY_TENANT_ID`) is authoritative for its own data scope: config,
    // not the request's acting tenant, decides where this workload writes. Using
    // it keeps the id and the ownership record workload-local even when a
    // full-platform token provisions on behalf of another tenant, and it matches
    // the filter boot hydration applies to the persisted `owners` rows (also
    // `AppConfig::tenant_namespace`) — so an API-provisioned company survives a
    // restart instead of being orphaned by a foreign-tenant prefix.
    //
    // Outside shared-single-DB mode the acting tenant is recorded, feeding
    // per-tenant quota and db-per-tenant / self-hosted ownership as before.
    // Canonicalized (bare-slug) so the recorded owner, the persisted `owners`
    // row, and quota counting all key by the same identity that tenant-scoped
    // auth compares a `tenant:acme` claim against.
    let tenant = crate::app::canonical_tenant(
        &state
            .config()
            .tenant_namespace
            .clone()
            .unwrap_or_else(|| acting_tenant(&GqlAuth::Platform(claims.clone()))),
    )
    .to_string();
    // Namespace the id with the workload's tenant so API-provisioned companies
    // are globally unique in one logical database (the same template name under
    // two tenant workloads no longer collides on the `companies` unique index).
    // A no-op when tenant-namespace mode is off; idempotent for an already-
    // prefixed explicit id.
    let id = state.config().namespaced_company_id(id);

    // Reject a duplicate id — checked against BOTH the live registry (a
    // company currently running) and the durable store (any company, live or
    // archived, that has ever existed under this id). Registry-only used to
    // miss the second case entirely: an archive removes a company from the
    // registry but never deletes its durable record, and `RuntimeBuilder::
    // build` loads any existing durable record for `id` before building over
    // it — so provisioning over an archived company's id, whether it is the
    // company this very request just reset or any OTHER company's old id
    // typed into Advanced, silently carried that record's old lifecycle,
    // ledger and overlays into the supposedly clean replacement instead of
    // being refused (issue #1828 comment 3865803905).
    //
    // The store lookup mirrors `RuntimeBuilder::build`'s own inherit-or-
    // construct fallback (`self.store.unwrap_or_else(FsCompanyStore::new)`)
    // so this check sees exactly the durable record the build below would
    // load, on every storage backend including the fs default, which never
    // populates `state.stores()`.
    if state.registry().get(&id).is_some() {
        return envelope(
            StatusCode::CONFLICT,
            "company_exists",
            &format!("company already exists: {id}"),
        );
    }
    let company_store: Arc<dyn CompanyStore> = match state.stores() {
        Some(stores) => stores.company.clone(),
        None => Arc::new(FsCompanyStore::new(state.home().to_path_buf())),
    };
    match company_store.load(&id).await {
        Ok(Some(_)) => {
            return envelope(
                StatusCode::CONFLICT,
                "company_exists",
                &format!("company already exists: {id}"),
            );
        }
        Ok(None) => {}
        Err(err) => return ApiError(err).into_response(),
    }

    // Quota: per-tenant then global.
    if let Some(max) = state.config().max_companies_per_tenant
        && state.tenant_company_count(&tenant) >= max
    {
        return envelope(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            &format!("tenant company quota of {max} reached"),
        );
    }
    if let Some(max) = state.config().max_companies
        && state.registry().len() >= max
    {
        return envelope(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            &format!("global company quota of {max} reached"),
        );
    }

    // The shared skill library, when this host serves one. A configured library
    // that cannot load is a server error rather than a silent empty registry —
    // provisioning a runtime that cannot heal its registry installs would hide
    // the misconfiguration until an agent came up skill-less.
    let skills_registry = match state.shared_skill_registry() {
        Ok(registry) => registry,
        Err(err) => return ApiError(err).into_response(),
    };

    // Build over the data dir, honoring the selected storage backend (fs
    // defaults when none is configured).
    let mut builder = RuntimeBuilder::new(state.home().to_path_buf(), manifest)
        .with_id(id.clone())
        // Issue #1739: a company provisioned after boot reports like one boot
        // registered. The host's tracker is process-wide and lives on the state
        // for exactly this reason — a second wiring path is a second place to
        // forget.
        .with_analytics(state.analytics())
        .with_tinyplace_api_url(state.config().tinyplace_api_url.clone())
        .with_host_base_url(state.config().host_base_url())
        // Issue #752: a provisioned tenant is a company like any other, so it
        // inherits the same repository-credential gates — which need to know
        // which backend is holding this host's secrets.
        .with_storage_kind(state.storage_kind())
        .with_skills_registry(skills_registry)
        // A host-wide sign-in mode set by setup (or flipped later) must reach
        // every company built from here on, including one provisioned after
        // that change — see `AppState::auth_mode_override`. Reuses the single
        // snapshot read above (`auth_mode_override`), not a fresh call: see
        // that binding's comment for why a second read here would be a race.
        .with_auth_mode_override(auth_mode_override);
    if let Some(stores) = state.stores() {
        builder = builder.with_stores(stores);
    }
    if let Some(overlay) = state.memory_overlay() {
        builder = builder.with_memory_overlay(&overlay);
    }
    // Issue #1050: the durable owner row is written BEFORE the company exists.
    //
    // It used to be written after, best-effort, and a failure was logged and
    // swallowed — so a transient write failure returned `201 Created` for a
    // company with no `owners` row. Boot hydration filters on exactly that row,
    // so after a restart the company was unattributable to its tenant: not
    // hydrated, addressable by nobody (`owner_of` → `None` → 403), and **missed
    // by a tenant-scoped purge or export**. Data surviving a deletion someone
    // believes they performed is the part that makes this more than untidiness.
    //
    // Ordering it first is what makes failing honest. Failing at the old site
    // could not be atomic: by then the runtime is built and registered, so
    // returning an error left exactly the orphan it meant to prevent, and there
    // is no company-deletion path to roll back with (`archive` is a lifecycle
    // transition plus registry removal, not a purge).
    //
    // This inverts the failure direction, deliberately: a crash between here
    // and a successful build leaves an `owners` row naming a company that was
    // never created. That is the strictly safer orphan — it hydrates a harmless
    // in-memory entry and can be removed, whereas a company with no owner row
    // hides data from a purge. Prefer the failure that leaves data visible over
    // the one that hides it.
    if let Some(ownership) = state.stores().and_then(|s| s.ownership.clone())
        && let Err(err) = persist_owner_with_retry(ownership.as_ref(), &id, &tenant).await
    {
        tracing::error!(
            company = %id,
            tenant = %tenant,
            error = %err,
            "refusing to provision: company ownership could not be persisted"
        );
        return envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "ownership_not_persisted",
            "could not record which tenant owns this company, so it was not created. \
             Nothing was provisioned — retry the request.",
        );
    }

    let runtime = match builder.build().await {
        Ok(runtime) => runtime,
        Err(err) => {
            // The owner row above now names a company that does not exist.
            // Best-effort removal keeps the reversed orphan from lingering; if
            // it fails the row is still the benign direction.
            if let Some(ownership) = state.stores().and_then(|s| s.ownership.clone())
                && let Err(cleanup) = ownership.remove_owner(&id).await
            {
                tracing::warn!(
                    company = %id,
                    error = %cleanup,
                    "provision failed and its ownership row could not be rolled back; the row \
                     names a company that was never created"
                );
            }
            return ApiError(err).into_response();
        }
    };

    // The none-mode-on-a-routable-bind refusal is checked earlier, before `id`
    // is resolved and before anything is persisted — see the comment there.
    // By construction `runtime.auth_mode()` cannot disagree with that check:
    // `RuntimeBuilder::build` resolves it from the exact same two inputs
    // (`self.auth_mode_override.unwrap_or_else(...)`), read from the same
    // `manifest` and the same host override, with no request-serialized
    // mutation of either in between.
    //
    // Registered BEFORE the `status()` read below, not after: `builder.build()`
    // already durably saved this company's `CompanyRecord` as one of its last
    // steps (see the duplicate-id check's comment above), so by this point the
    // company genuinely exists — `status()` only re-reads that same record for
    // the response body. Registering first means a transient failure in that
    // read (a store blip right after the write) still leaves the company live
    // and addressable: the in-memory registry, not this request's response,
    // is what the duplicate-id check and every other route key existence off
    // of. Registering only after a successful `status()` left exactly that
    // failure unrecoverable — the durable record made a retry's duplicate
    // check say `company_exists`, while the registry never got the runtime
    // that would make `company_exists` true, so no request could ever create
    // OR address that id again (issue #1828 comment 3866132497).
    let status = match register_and_report_status(&state, &id, &tenant, runtime).await {
        Ok(status) => status,
        Err(err) => return ApiError(err).into_response(),
    };

    (StatusCode::CREATED, Json(status)).into_response()
}

/// Registers `runtime` and records its ownership, then reads its status for
/// the response body.
///
/// Registration happens first, deliberately — see the comment at the call
/// site. A `status()` failure after this point still returns `Err` (the
/// caller sees an error response), but by then the company is already live
/// and addressable through `state.registry()`, exactly as if the read had
/// succeeded: nothing downstream of this function can tell the two cases
/// apart. Split out so that property is directly testable without a full
/// HTTP round trip (`test.rs`).
async fn register_and_report_status(
    state: &AppState,
    id: &CompanyId,
    tenant: &str,
    runtime: crate::runtime::CompanyRuntime,
) -> crate::Result<crate::runtime::types::CompanyStatus> {
    // Issue #1739: on a host provisioned into an empty registry, boot had no
    // runtime to read cognition from and the analytics envelope recorded the
    // default descriptor (`custom`/`unknown`). Now there is one, so relabel it
    // — otherwise every event this tenant ever sends is mislabeled.
    state.analytics().observe_cognition(runtime.cognition());
    let runtime = std::sync::Arc::new(runtime);
    state.registry().insert(id.clone(), runtime.clone());
    // The durable row was written and verified above, before the build, so this
    // is only the in-memory mirror the running process serves from (issue
    // #1050).
    state.set_owner(id.clone(), tenant.to_string());

    runtime.status().await
}

/// How many times [`persist_owner_with_retry`] attempts the durable write.
///
/// The failure this exists for is transient — a mongo blip, an election, a
/// timeout — so a couple of extra attempts turn most would-be provision
/// failures back into successes. Small on purpose: a caller is waiting on this
/// request, and a backend that is genuinely down should be reported as down
/// rather than held open.
const OWNERSHIP_WRITE_ATTEMPTS: usize = 3;

/// Writes the company → tenant row, retrying a transient failure (issue #1050).
///
/// Returns the last error if every attempt fails, which the caller turns into a
/// refusal to provision. There is no sleep between attempts: the retry is here
/// for a failed round-trip rather than a busy one, and a request-scoped backoff
/// would hold the caller open for a backend that is not coming back inside this
/// request anyway.
async fn persist_owner_with_retry(
    ownership: &dyn crate::store::select::OwnershipStore,
    id: &CompanyId,
    tenant: &str,
) -> crate::Result<()> {
    let mut last = None;
    for attempt in 1..=OWNERSHIP_WRITE_ATTEMPTS {
        match ownership.set_owner(id, tenant).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!(
                    company = %id,
                    attempt,
                    error = %err,
                    "persisting company ownership failed; retrying"
                );
                last = Some(err);
            }
        }
    }
    Err(last.expect("at least one attempt ran"))
}

/// How many times [`archive_reconcile_status`] retries a transient
/// `status()` failure. Mirrors [`OWNERSHIP_WRITE_ATTEMPTS`]'s reasoning: this
/// read is the last chance to catch a genuinely-landed archive after
/// `transition` already reported non-`OK`, and a lone blip should not be
/// mistaken for "not archived" when a couple of retries would resolve it.
const ARCHIVE_RECONCILE_READ_ATTEMPTS: usize = 3;

/// Re-reads `runtime.status()` for `archive`'s non-`OK` reconciliation
/// branch, retrying a transient failure instead of trusting a single `Err`.
///
/// `set_lifecycle` persists `lifecycle: "archived"` to the store BEFORE it
/// appends the `LifecycleChanged` audit event (`src/company/runtime.rs`), so
/// by the time this runs the write has already landed — a failure here is a
/// read problem, not evidence the archive didn't happen. Collapsing that
/// straight to "not archived" (the pre-fix `unwrap_or(false)`) had no other
/// trigger to retry it: this reconciliation branch only runs once per
/// request, and the create/reset dialog's own client-side reconciliation
/// (`create-company-dialog.tsx`) has no visibility into the server registry —
/// if ITS later status lookup succeeds and observes `archived`, it reports
/// the reset as done and never calls `archive` again, so registry/owner
/// cleanup was permanently skipped by a single passing blip (issue #1828
/// comment 3875297944). No sleep between attempts, matching
/// `persist_owner_with_retry`: this is for a failed round-trip, not a busy
/// one, and a caller is waiting on this request.
async fn archive_reconcile_status(
    runtime: &std::sync::Arc<crate::runtime::CompanyRuntime>,
) -> crate::Result<crate::runtime::types::CompanyStatus> {
    let mut last = None;
    for attempt in 1..=ARCHIVE_RECONCILE_READ_ATTEMPTS {
        match runtime.status().await {
            Ok(status) => return Ok(status),
            Err(err) => {
                tracing::warn!(
                    attempt,
                    error = %err,
                    "archive reconciliation status() read failed; retrying"
                );
                last = Some(err);
            }
        }
    }
    Err(last.expect("at least one attempt ran"))
}

// ---------------------------------------------------------------------------
// Lifecycle controls
// ---------------------------------------------------------------------------

/// The actor recorded for a platform/operator-driven lifecycle transition.
/// Who a lifecycle transition is recorded as.
///
/// A human is recorded as themselves; a machine credential as the tenant it
/// acts for. Previously everything was `Operator`, because that was the only
/// principal that could reach these routes.
fn lifecycle_actor(auth: &GqlAuth) -> Actor {
    match auth {
        GqlAuth::User(user) => Actor {
            kind: ActorKind::User,
            id: user.user_id.clone(),
        },
        GqlAuth::Platform(_) => Actor {
            kind: ActorKind::Operator,
            id: acting_tenant(auth),
        },
    }
}

/// Whether the request carries `?reason=budget`, without pulling in axum's
/// `query` feature.
fn reason_is_budget(uri: &Uri) -> bool {
    uri.query()
        .map(|q| q.split('&').any(|pair| pair == "reason=budget"))
        .unwrap_or(false)
}

/// Applies a lifecycle transition to the company registered under `id`,
/// returning the fresh status.
async fn transition(state: &AppState, auth: &GqlAuth, id: &CompanyId, to: &str) -> Response {
    let Some(runtime) = state.registry().get(id) else {
        return not_found(id.as_ref());
    };
    transition_runtime(&runtime, auth, to).await
}

/// Applies a lifecycle transition directly to an already-resolved `runtime`,
/// returning the fresh status.
///
/// A caller holding an authorized runtime (e.g. [`AdminScopedCompany`]) must
/// use this rather than [`transition`]: looking `id` back up in the registry
/// can return a different runtime than the one authorization approved, if a
/// rebuild swap lands in between.
async fn transition_runtime(
    runtime: &crate::runtime::CompanyRuntime,
    auth: &GqlAuth,
    to: &str,
) -> Response {
    if let Err(err) = runtime.set_lifecycle(to, lifecycle_actor(auth)).await {
        return ApiError(err).into_response();
    }
    match runtime.status().await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(err) => ApiError(err).into_response(),
    }
}

/// `POST /api/v1/companies/{id}/pause` — stop accepting work (admin-scoped).
async fn pause(
    admin: AdminScopedCompany,
    crate::server::platform_auth::CompanyAuth(auth): crate::server::platform_auth::CompanyAuth,
    State(state): State<AppState>,
    uri: Uri,
) -> Response {
    let response = transition_runtime(&admin.runtime, &auth, "paused").await;
    // A budget-triggered pause emits the `budget.exhausted` webhook.
    if response.status() == StatusCode::OK && reason_is_budget(&uri) {
        emit_budget_exhausted(&state, admin.id()).await;
    }
    response
}

/// `POST /api/v1/companies/{id}/resume` — resume accepting work (admin-scoped).
async fn resume(
    admin: AdminScopedCompany,
    crate::server::platform_auth::CompanyAuth(auth): crate::server::platform_auth::CompanyAuth,
) -> Response {
    // `suspended` is a platform-forced pause (billing/abuse); only a
    // platform-scope caller may lift it. Neither an owner token nor a company's
    // own admin may resume a company the platform suspended.
    let platform = matches!(&auth, GqlAuth::Platform(c) if c.has_platform_scope());
    if !platform {
        match admin.runtime.status().await {
            Ok(status) if status.lifecycle == "suspended" => {
                return crate::server::platform_auth::forbidden();
            }
            Ok(_) => {}
            Err(err) => return ApiError(err).into_response(),
        }
    }
    transition_runtime(&admin.runtime, &auth, "running").await
}

// ---------------------------------------------------------------------------
// Emergency stop (issue #86)
// ---------------------------------------------------------------------------

/// The confirmation phrase `emergency-pause` requires in its body.
///
/// A fixed phrase, not the company id: engaging the stop is the *safe*
/// direction, and an operator reaching for a panic button under pressure should
/// not have to look up an id to make it work. The step-up here is only to stop a
/// stray click, so it is deliberately weaker than the release below.
const PAUSE_CONFIRMATION: &str = "EMERGENCY-PAUSE";

/// The step-up body both emergency routes take.
#[derive(Debug, Deserialize)]
struct EmergencyBody {
    /// The confirmation phrase. See [`PAUSE_CONFIRMATION`] and
    /// [`emergency_resume`] for what each route expects.
    #[serde(default)]
    confirm: String,
    /// An optional operator note, journaled with the event.
    #[serde(default)]
    reason: Option<String>,
}

/// Rejects a request whose confirmation phrase does not match `expected`.
///
/// A plain byte comparison of two operator-supplied short strings — no model
/// judgement, no fuzzy matching, no normalisation beyond trimming surrounding
/// whitespace a form would add. The response names what was expected, because
/// this is a confirmation step and not a secret; hiding it would just make the
/// panic button hard to press in an emergency.
fn confirmation_error(supplied: &str, expected: &str) -> Option<Response> {
    if supplied.trim() == expected {
        return None;
    }
    Some(envelope(
        StatusCode::BAD_REQUEST,
        "confirmation_required",
        &format!("this action requires {{\"confirm\": \"{expected}\"}} in the request body"),
    ))
}

/// `POST /api/v1/companies/{id}/emergency-pause` — the governance kill switch
/// (admin-scoped, issue #86).
///
/// The confirmation phrase below is a step-up against a stray click, not an
/// authority check: it is a fixed, published string. Authority is
/// [`AdminScopedCompany`] in the signature.
///
/// Denies every new effect outside `EffectGroup::Other` until an operator
/// deliberately releases it. Distinct from `/pause`, which stops the company
/// *including chat* by moving `lifecycle`; this leaves the lifecycle untouched
/// so the operator can keep asking the company what it was doing.
///
/// Idempotent: pressing it twice returns `200` with `changed: false` rather than
/// an error. A panic button that punishes a second press is a bad panic button.
async fn emergency_pause(
    admin: AdminScopedCompany,
    crate::server::platform_auth::CompanyAuth(auth): crate::server::platform_auth::CompanyAuth,
    body: Result<Json<EmergencyBody>, JsonRejection>,
) -> Response {
    // A missing, empty, or malformed JSON body all read as "no step-up was
    // supplied" and fall through to the same `confirmation_required` envelope a
    // request with an absent body already gets — a panic button has to tell the
    // operator *what* to send, not answer with an opaque `Json` rejection.
    let body = body.ok().map(|Json(b)| b).unwrap_or(EmergencyBody {
        confirm: String::new(),
        reason: None,
    });
    if let Some(resp) = confirmation_error(&body.confirm, PAUSE_CONFIRMATION) {
        return resp;
    }
    let runtime = &admin.runtime;
    match runtime
        .emergency_pause(lifecycle_actor(&auth), body.reason)
        .await
    {
        Ok(changed) => emergency_response(runtime, changed, None).await,
        Err(err) => {
            emergency_response(
                runtime,
                false,
                Some(format!(
                    "the emergency stop is active in memory but its journal \
                 write failed and will not survive a restart: {err}"
                )),
            )
            .await
        }
    }
}

/// `POST /api/v1/companies/{id}/emergency-resume` — release the kill switch
/// (admin-scoped, issue #86).
///
/// **The confirmation is the company's own id**, which is a deliberately
/// stronger step-up than the fixed phrase `emergency-pause` takes. Releasing is
/// the unsafe direction: it is the only way out of the stop, so it should not be
/// reachable by replaying the same body against a different company, and typing
/// the id is the standard way to make an operator name what they are about to
/// restart.
///
/// There is no timeout anywhere in this path. A stop persists until this
/// endpoint is called by an identified operator, across restarts included.
async fn emergency_resume(
    admin: AdminScopedCompany,
    crate::server::platform_auth::CompanyAuth(auth): crate::server::platform_auth::CompanyAuth,
    body: Result<Json<EmergencyBody>, JsonRejection>,
) -> Response {
    // Same contract as `emergency_pause`: a missing, empty, or malformed body
    // reads as "no step-up was supplied" and answers with the documented
    // `confirmation_required` envelope rather than a bare `Json` rejection.
    let body = body.ok().map(|Json(b)| b).unwrap_or(EmergencyBody {
        confirm: String::new(),
        reason: None,
    });
    if let Some(resp) = confirmation_error(&body.confirm, admin.id().as_ref()) {
        return resp;
    }
    let runtime = &admin.runtime;
    match runtime
        .emergency_resume(lifecycle_actor(&auth), body.reason)
        .await
    {
        Ok(changed) => emergency_response(runtime, changed, None).await,
        Err(err) => {
            emergency_response(
                runtime,
                false,
                Some(format!(
                    "the emergency stop is still engaged in memory but its journal \
                 write failed and will not survive a restart: {err}"
                )),
            )
            .await
        }
    }
}

/// The shared success body: the company's status plus whether this call was the
/// one that moved the switch. `message` is only present on the degraded arm —
/// when the in-memory transition happened but its journal append failed, so the
/// operator gets the real `emergency_paused` state plus the caveat instead of a
/// bare error that reads as "nothing happened".
async fn emergency_response(
    runtime: &std::sync::Arc<crate::runtime::CompanyRuntime>,
    changed: bool,
    message: Option<String>,
) -> Response {
    match runtime.status().await {
        Ok(status) => {
            let mut body = match serde_json::to_value(&status) {
                Ok(serde_json::Value::Object(map)) => map,
                _ => serde_json::Map::new(),
            };
            body.insert("changed".into(), json!(changed));
            if let Some(message) = message {
                body.insert("message".into(), json!(message));
            }
            (StatusCode::OK, Json(serde_json::Value::Object(body))).into_response()
        }
        Err(err) => ApiError(err).into_response(),
    }
}

/// `POST /api/v1/companies/{id}/suspend` — park a company (platform-scoped).
async fn suspend(
    PlatformScope(claims): PlatformScope,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let id = CompanyId::new(id);
    let auth = GqlAuth::Platform(claims);
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return resp;
    }
    transition(&state, &auth, &id, "suspended").await
}

/// `POST /api/v1/companies/{id}/archive` — terminally archive a company and
/// remove it from the registry (platform-scoped).
async fn archive(
    PlatformScope(claims): PlatformScope,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let id = CompanyId::new(id);
    let auth = GqlAuth::Platform(claims);
    if let Some(resp) = authorize_address(&state, &auth, &id) {
        return resp;
    }
    let response = transition(&state, &auth, &id, "archived").await;
    // `response.status() == OK` is sufficient proof on its own: `transition`
    // already re-read `status()` after `set_lifecycle` and its body already
    // confirms `lifecycle: "archived"`. Re-reading a THIRD time to
    // reconfirm what the response already proved is pure downside — a
    // transient failure on that redundant read must not undo a response
    // that already succeeded (issue #1828 comment 3875203599).
    //
    // The extra read below is reserved for reconciling a non-`OK` response.
    // `CompanyRuntime::set_lifecycle` (`src/company/runtime.rs`) writes
    // `lifecycle: "archived"` to the store BEFORE it appends the
    // `LifecycleChanged` audit event, and `transition` above then re-reads
    // `status()` after that — so a failure in either the event append or
    // that re-read surfaces here as a non-200 `response` even though the
    // durable record really is archived. Re-reading status directly, the
    // same way the create/reset console dialog's own reconciliation does
    // client-side (`create-company-dialog.tsx`, commit 1191ad67e), catches
    // both failure points so an already-archived company is never left
    // registered — still occupying its tenant/global quota slot and still
    // showing up in `listCompanies()` — for a request whose write already
    // succeeded (issue #1828 comment 3875046440).
    //
    // That reconciliation read itself retries a transient failure
    // (`archive_reconcile_status` below) instead of collapsing a single `Err`
    // straight to "not archived". This branch is the LAST place that can ever
    // trigger cleanup: the create/reset dialog's own client-side
    // reconciliation has no visibility into the server registry, so if ITS
    // later status lookup succeeds and observes `archived`, it considers the
    // reset done and never calls `archive` again — permanently skipping
    // cleanup a single-shot blip on this read would otherwise have caused
    // (issue #1828 comment 3875297944).
    // The runtime confirmed archived, not just the id — `evict_archived_company`
    // needs the specific instance so it can refuse to remove a replacement that
    // has since taken this id over (a rebuild swap; see that function's comment).
    let archived_runtime: Option<Arc<crate::runtime::CompanyRuntime>> =
        if response.status() == StatusCode::OK {
            state.registry().get(&id)
        } else {
            match state.registry().get(&id) {
                Some(runtime) => {
                    let confirmed = archive_reconcile_status(&runtime)
                        .await
                        .map(|status| status.lifecycle == "archived")
                        .unwrap_or(false);
                    confirmed.then_some(runtime)
                }
                None => None,
            }
        };
    if let Some(runtime) = archived_runtime {
        evict_archived_company(&state, &id, &runtime).await;
    }
    response
}

/// Removes an archived company from the live registry and drops both its
/// in-memory and persisted ownership rows.
///
/// The single implementation of archive's post-transition cleanup, shared by
/// [`archive`] and [`RegistryEvictor`] (the maintenance-loop hook) so
/// the inline path and the stranded-cleanup retry cannot drift.
///
/// Two orderings matter here, each guarding a different way this call could
/// destroy something it never actually confirmed (codex review on #1943, PR
/// comments 3894439351 and 3894439358):
///
/// - **The persisted ownership row is removed BEFORE the registry entry and
///   the in-memory owner, not after.** `MaintenanceTicker::tick` only
///   retries a company it can still see in [`CompanyRegistry::list`]
///   (`src/runtime/maintenance.rs`) — so a company de-registered here with
///   its persisted-ownership deletion still failed can never be revisited.
///   Failing closed (leaving the company registered) instead lets the next
///   tick retry the whole eviction; failing the old way (registry gone,
///   ownership row still failed to delete) orphans that row forever with
///   nothing left in the registry to trigger a retry against it.
/// - **The registry removal is conditional on `expected`, not "whatever is
///   at `id`".** [`CompanyRegistry::insert`] is the one choke point every
///   registration goes through, and it replaces an already-occupied slot
///   unconditionally — a rebuild swap (`runtime::rebuild::rebuild_company`)
///   is the production caller that can land a fresh runtime under this same
///   id in the window between a caller observing `"archived"` and this call
///   actually running. Removing by id alone would deregister that live
///   replacement instead of doing nothing. The ownership rows above stay
///   unconditional by id regardless: `POST /api/v1/companies` refuses
///   `company_exists` for any id still registered (`provision`, this file),
///   so the only thing that can ever replace an already-registered id is a
///   rebuild of the SAME company — same owner, same durable lifecycle
///   (`CompanyRuntime::status` reads it from the handed-over store, so a
///   rebuild successor of an archived company reports `"archived"` too) —
///   never a different company's row.
pub(crate) async fn evict_archived_company(
    state: &AppState,
    id: &CompanyId,
    expected: &Arc<crate::runtime::CompanyRuntime>,
) {
    let ownership = state.stores().and_then(|s| s.ownership.clone());
    if evict_registry_and_ownership(state.registry(), &ownership, id, expected).await {
        state.remove_owner(id);
    }
}

/// The sequencing `evict_archived_company`'s doc comment above describes,
/// pulled out of `AppState` so the ordering is unit-testable against a
/// lightweight [`OwnershipStore`](crate::store::select::OwnershipStore) fake
/// and a bare [`CompanyRegistry`] instead of a full `StorageHandles`.
///
/// Returns whether the persisted ownership row was successfully removed (or
/// there was none configured) — the caller's cue to also clear the
/// AppState-level in-memory owner cache, which stays unconditional by id
/// regardless of the registry outcome below (see the "same company" argument
/// in `evict_archived_company`'s doc comment).
async fn evict_registry_and_ownership(
    registry: &crate::runtime::CompanyRegistry,
    ownership: &Option<Arc<dyn crate::store::select::OwnershipStore>>,
    id: &CompanyId,
    expected: &Arc<crate::runtime::CompanyRuntime>,
) -> bool {
    if let Some(ownership) = ownership
        && let Err(err) = ownership.remove_owner(id).await
    {
        tracing::warn!(
            company = %id,
            error = %err,
            "failed to remove persisted ownership; leaving the company registered for a later maintenance retry"
        );
        return false;
    }
    if registry.remove_if(id, expected).is_none() {
        tracing::info!(
            company = %id,
            "kept the registered runtime — it was replaced since being observed archived"
        );
    }
    true
}

/// The production [`CompanyEvictor`](crate::runtime::maintenance::CompanyEvictor):
/// runs archive's cleanup against `AppState` for a company the maintenance loop
/// found archived but still registered.
pub struct RegistryEvictor {
    state: AppState,
}

impl RegistryEvictor {
    /// Builds an evictor over `state`.
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl crate::runtime::maintenance::CompanyEvictor for RegistryEvictor {
    async fn evict(&self, company: &CompanyId, expected: &Arc<crate::runtime::CompanyRuntime>) {
        evict_archived_company(&self.state, company, expected).await;
    }
}

// ---------------------------------------------------------------------------
// Webhook emission (shared with the operator chat surface)
// ---------------------------------------------------------------------------

/// Emits the webhooks a completed cycle implies: one `approval.requested` per
/// newly parked approval, and one `work.completed` when the cycle produced
/// output. A no-op when no webhook is configured.
pub(crate) async fn emit_cycle_webhooks(state: &AppState, id: &CompanyId, report: &CycleReport) {
    let Some(webhook) = state.webhook() else {
        return;
    };
    for approval_id in &report.parked {
        let event = WebhookEvent::now(
            WebhookKind::ApprovalRequested,
            id.clone(),
            json!({ "approval_id": approval_id.as_ref() }),
        );
        webhook.emit(&event).await;
    }
    if !report.responses.is_empty() {
        let event = WebhookEvent::now(
            WebhookKind::WorkCompleted,
            id.clone(),
            json!({ "responses": report.responses.len() }),
        );
        webhook.emit(&event).await;
    }
}

/// Emits a `feedback.created` webhook. A no-op when no webhook is configured.
pub(crate) async fn emit_feedback_webhook(state: &AppState, id: &CompanyId, note: &str) {
    let Some(webhook) = state.webhook() else {
        return;
    };
    let event = WebhookEvent::now(
        WebhookKind::FeedbackCreated,
        id.clone(),
        json!({ "note": note }),
    );
    webhook.emit(&event).await;
}

/// Emits a `budget.exhausted` webhook. A no-op when no webhook is configured.
async fn emit_budget_exhausted(state: &AppState, id: &CompanyId) {
    let Some(webhook) = state.webhook() else {
        return;
    };
    let event = WebhookEvent::now(WebhookKind::BudgetExhausted, id.clone(), json!({}));
    webhook.emit(&event).await;
}

#[cfg(test)]
mod test;
