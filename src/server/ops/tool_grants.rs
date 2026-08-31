//! The tool-grant write plane: `GET`/`PUT`/`DELETE {scope}/tools/grants`
//! (issue #1796).
//!
//! ## The dead end this ends
//!
//! Connecting an integration and granting its tool namespace are two separate
//! steps, and only the first one had a write path. The console could store a
//! Chargebee key, a PayPal client secret, a hosting token, a search key or a
//! Composio token — and then had to tell the operator, accurately, that the
//! `[tools].allow` grant "cannot be fixed from this page". So five surfaces all
//! reported **Connected** over a harness that had wired nothing, and there was
//! no page anywhere in the product that could fix it.
//!
//! Editing `company.toml` was not the escape it sounds like: on a hosted tenant
//! the manifest is a read-only boot snapshot baked into the image, so for those
//! companies the grant was unreachable by any means at all.
//!
//! ## Why this writes an overlay and not the manifest
//!
//! The same reason [`policy`](super::policy) does. A rebuild re-persists
//! `record.manifest` **from the seed**, merging only `[workflows].enabled`;
//! *"every other manifest field is seed-authoritative, and for `[tools]` /
//! `[policy]` that is a security property"* (`runtime::builder`). So this route
//! writes [`ToolGrantsOverride`], and
//! `runtime::builder::carry_tool_grants_override` drops it wholesale the moment
//! version control edits `[tools]` — version control still wins when it speaks.
//!
//! ## Why the namespace list is closed
//!
//! This is the only overlay in the product that **widens** capability, so what
//! it may widen is bounded by [`CONSOLE_GRANTABLE_NAMESPACES`] rather than by
//! what an operator can type. Every entry is a namespace the catch-all `*`
//! deliberately refuses to confer *and* one the console holds a credential form
//! for — so granting is the second half of an action the operator already took
//! against an account they already proved they hold. `shell`, `code` and `web`
//! have no such form, and a settings page that could confer them would be the
//! general capability-widening surface the seed-wins rule exists to prevent.
//!
//! The list is enforced twice: here, and again in
//! [`CompanyRecord::effective_tool_allow`], so a value that reached the store
//! some other way can never confer `shell`.
//!
//! ## Admin-only, and attributed
//!
//! Widening what the company's agents can reach is at least as sharp a
//! privilege boundary as loosening the approval tier, so both writes go through
//! [`require_admin`](crate::server::users::admin::require_admin) and stamp who
//! did it and when. The console renders that attribution.
//!
//! ## When a grant takes effect — and why that has three answers
//!
//! Tool belts are wired once per roster build, not read per call, so storing a
//! grant is not the same as delivering one. Which of three things happens
//! depends on the company's cognition path, and the response says which:
//!
//! - **Harness path** — `HarnessPool::ensure` rebuilds the roster when its
//!   grant fingerprint moves (`tool_grants_fingerprint`), so the write lands and
//!   the next turn runs with the tools. An in-flight turn finishes with the belt
//!   it started with.
//! - **Hosted / sidecar / echo** — there is no `ensure`. `RuntimeBuilder::build`
//!   snapshotted the provider's grants and the hosted brain's `tool_catalog` at
//!   construction, so [`apply_to_runtime`] rebuilds the runtime in place through
//!   the issue-#290 seam `PUT …/inference` uses.
//! - **Neither available** — no rebuilder wired, or the rebuild failed. The
//!   response says **restart**, because that is the truth.
//!
//! The third case is the one worth being strict about. Reporting "next turn" to
//! a company that will not pick the grant up until a restart would be this
//! page asserting reach the runtime does not deliver — the whole of #1796,
//! reproduced one layer inside its own fix.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::OpenCompanyError;
use crate::ports::now_millis;
use crate::ports::store::company_write_lock;
use crate::ports::types::{
    Actor, ActorKind, CONSOLE_GRANTABLE_NAMESPACES, CompanyRecord, ToolGrantsOverride,
    console_grantable, effective_tool_allow, seed_tool_allow,
};
use crate::server::error::ApiError;
use crate::server::ops::{ScopedCompany, scoped};
use crate::server::users::admin::require_admin;

/// What the console tells an operator about when a grant starts working, on a
/// company whose tool belts are re-resolved live.
///
/// A constant rather than prose invented at the call site, so the routes cannot
/// describe the timing differently.
pub(crate) const TAKES_EFFECT: &str =
    "on the next turn — a turn already running finishes with the tools it started with";

/// The same answer for a company whose runtime had to be rebuilt to pick the
/// grant up (see [`apply_to_runtime`]).
pub(crate) const TAKES_EFFECT_REBUILT: &str =
    "now — this company's runtime was rebuilt so its teammates hold the new tools";

/// And the honest answer when neither is true: the grant is stored, and the
/// belts this company is actually running were fixed when it booted.
///
/// Never softened into one of the two above. A grant that reports "next turn"
/// on a runtime that will not pick it up until a restart is the same lie this
/// whole change exists to end, one layer in — the console would be asserting
/// reach the runtime does not deliver.
pub(crate) const TAKES_EFFECT_RESTART: &str = "on the next restart — this company's teammates were built with a fixed tool belt \
     and this host cannot rebuild it in place";

/// Builds the tool-grant route fragment.
pub fn router() -> Router<AppState> {
    scoped(
        "/tools/grants",
        get(read_grants).put(add_grant).delete(clear_grants),
    )
}

/// The company's effective tool grants, and what the console may add to them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolGrantsDto {
    /// The grant list actually in force — the seed's `[tools].allow` plus the
    /// namespaces granted from the console, in that order.
    pub(crate) allow: Vec<String>,
    /// The seed's own list, so the console can name what a reset would restore
    /// and show which grants version control is responsible for.
    pub(crate) manifest_allow: Vec<String>,
    /// The namespaces this route added, in the order they were granted.
    pub(crate) added: Vec<String>,
    /// Every namespace this route will accept — the console renders a control
    /// only for these, so a build that widens the list needs no console change.
    pub(crate) grantable: Vec<&'static str>,
    /// Who granted, if anything was granted here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) set_by: Option<String>,
    /// When (epoch millis), if anything was granted here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) set_at_millis: Option<u64>,
    /// When a grant starts working, in the host's words.
    ///
    /// Resolved per response rather than fixed, because the answer genuinely
    /// differs by cognition path — see [`apply_to_runtime`].
    pub(crate) takes_effect: &'static str,
}

impl ToolGrantsDto {
    /// The read shape, with the live-refresh timing (`GET`, and any write that
    /// stored nothing). A write that had to reach the runtime reports what
    /// actually happened via [`Self::build_with`].
    pub(crate) fn build(record: &CompanyRecord) -> Self {
        Self::build_with(record, TAKES_EFFECT)
    }

    pub(crate) fn build_with(record: &CompanyRecord, takes_effect: &'static str) -> Self {
        let held = record.overlay_tool_grants.as_ref();
        Self {
            allow: record.effective_tool_allow(),
            // The record's manifest is materialised seed-plus-grants (see the
            // fold in `runtime::builder`), so the seed's own list is recovered
            // by removing what this route added. Reporting the materialised
            // list here would tell an operator version control grants something
            // it does not, and a `DELETE` would then look like it had done
            // nothing.
            manifest_allow: seed_tool_allow(&record.manifest.tools.allow, held),
            added: held.map(|h| h.added.clone()).unwrap_or_default(),
            grantable: CONSOLE_GRANTABLE_NAMESPACES.to_vec(),
            set_by: held.map(|h| h.set_by.id.clone()),
            set_at_millis: held.map(|h| h.at_millis),
            takes_effect,
        }
    }
}

/// Makes a stored grant **live**, and reports which of the three timings the
/// caller actually got.
///
/// # Why a write route has to do anything at all
///
/// Tool belts are not read per call. `RuntimeBuilder::build` snapshots the
/// provider's grant list and the hosted brain's `tool_catalog` from the
/// manifest at construction, and only one path re-resolves them afterwards:
/// `HarnessPool::ensure`, which rebuilds the roster when its grant fingerprint
/// moves. That path lives behind the `openhuman` feature and serves companies
/// on the **harness** cognition path.
///
/// A company on the hosted, sidecar or echo path has no `ensure`. Saving the
/// record there leaves every status route reporting `granted: true` over belts
/// that were fixed at boot — the connect page would once again promise reach
/// the runtime does not deliver, which is the whole of #1796 reproduced inside
/// its own fix.
///
/// So: harness companies need nothing (the fingerprint carries it). Everyone
/// else gets an in-place rebuild through the same seam `PUT …/inference` uses
/// for the same reason (issue #290) — and when this host has no rebuilder
/// wired, or the rebuild fails, the response says **restart** rather than
/// quietly claiming the next turn.
///
/// A failed rebuild is deliberately not an error: the grant is stored and
/// durable either way, and the company is running exactly what
/// `TAKES_EFFECT_RESTART` describes. Reporting that honestly is a better
/// outcome than a 500 over a write that landed.
async fn apply_to_runtime(state: &AppState, company: &ScopedCompany) -> &'static str {
    if company.runtime.cognition().path == crate::ports::brain::HARNESS_PATH {
        return TAKES_EFFECT;
    }
    if !state.can_rebuild_in_place() {
        return TAKES_EFFECT_RESTART;
    }
    match crate::runtime::rebuild_company(state, company.id()).await {
        Ok(_) => TAKES_EFFECT_REBUILT,
        Err(err) => {
            tracing::warn!(
                company = %company.id(),
                error = %err,
                "[tools] the grant was stored but this company's runtime could not be rebuilt; \
                 its teammates keep the belt they booted with until a restart"
            );
            TAKES_EFFECT_RESTART
        }
    }
}

/// The add-grant body.
///
/// One namespace per call rather than a whole list, because that is the shape
/// of the action: an operator on the Chargebee panel is granting `chargebee`,
/// and a list-shaped body would make "add this one" express itself as "replace
/// the set with these", which is a different and revocable operation this
/// endpoint deliberately does not offer.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddGrant {
    namespace: String,
}

/// The remove-grant query.
///
/// `DELETE …/tools/grants` clears everything this route granted;
/// `?namespace=paypal` withdraws exactly one, which is what the console's
/// "remove" control on a single panel means.
#[derive(Debug, Deserialize)]
struct ClearQuery {
    #[serde(default)]
    namespace: Option<String>,
}

/// `GET {scope}/tools/grants` — the grants in force, the seed's own, and what
/// the console may add.
///
/// Not admin-gated, matching every other connection status route: a
/// non-admin already sees the `granted` flag on each of them, and a
/// capability list is not a secret. The **writes** are admin-only.
async fn read_grants(
    company: ScopedCompany,
) -> Result<Json<ToolGrantsDto>, crate::server::Rejection> {
    let record = load_record(&company).await?;
    Ok(Json(ToolGrantsDto::build(&record)))
}

/// `PUT {scope}/tools/grants` — grant one namespace. Admin-only, attributed,
/// in force on the next turn.
async fn add_grant(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    Json(body): Json<AddGrant>,
) -> Result<Json<ToolGrantsDto>, crate::server::Rejection> {
    let admin = require_admin(&headers, &state, &company.runtime, peer).await?;

    let namespace = body.namespace.trim().to_ascii_lowercase();
    if !console_grantable(&namespace) {
        return Err(refusal(&format!(
            "`{namespace}` is not a namespace this page may grant. \
             Grantable from the console: {}. Anything else is granted by \
             editing `[tools].allow` in the company's manifest.",
            CONSOLE_GRANTABLE_NAMESPACES.join(", ")
        ))
        .into());
    }

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;

    // Already granted by version control: store nothing. An override that
    // duplicates the seed is not harmless here the way a redundant policy
    // override is — it would render in the console as "granted from this page",
    // and a later `DELETE` would then appear to revoke a seed grant it cannot
    // touch. Returning the current state is the honest answer to "make sure
    // this is granted", which is what the caller asked.
    let seed = seed_tool_allow(
        &record.manifest.tools.allow,
        record.overlay_tool_grants.as_ref(),
    );
    if seed.contains(&namespace) {
        return Ok(Json(ToolGrantsDto::build(&record)));
    }

    // Already granted from the console: return what stands, and store nothing.
    //
    // Re-storing would leave the list identical and stamp a NEW `set_by` and
    // `at_millis` over it, so a retried `PUT` — a client whose response was lost,
    // a second admin making sure — would re-attribute the original operator's
    // grant to whoever asked last. Attribution is the whole reason this override
    // carries an actor: a capability widening that records the wrong person is
    // barely better than one that records nobody. The grant happened once, and
    // the record should say when and by whom.
    let mut added = record
        .overlay_tool_grants
        .as_ref()
        .map(|held| held.added.clone())
        .unwrap_or_default();
    if added.contains(&namespace) {
        return Ok(Json(ToolGrantsDto::build(&record)));
    }
    added.push(namespace);

    record.overlay_tool_grants = Some(ToolGrantsOverride {
        added,
        set_by: Actor {
            kind: ActorKind::User,
            id: admin.user_id,
        },
        at_millis: now_millis(),
    });
    // Fold into the persisted manifest immediately, exactly as the rebuild does.
    // Every reader of "what does this company grant" — the roster build, the
    // harness tool wiring, all five connection status routes — reads
    // `[tools].allow` off the manifest, so folding here is what makes the grant
    // true for all of them at once rather than true only where somebody
    // remembered to resolve an overlay.
    record.manifest.tools.allow = record.effective_tool_allow();

    save(&company, &record).await?;
    // Release the write lock before `apply_to_runtime`, which may call
    // `rebuild_company` (PR #1875 review finding): the rebuild's own
    // load-through-save of the record now serializes on this same lock, and
    // this task is still holding it — a non-reentrant `tokio::sync::Mutex`, so
    // keeping it would deadlock this handler against its own rebuild rather
    // than protect anything. The save above already landed under the lock;
    // nothing past this point still needs it held.
    drop(_lock);
    let takes_effect = apply_to_runtime(&state, &company).await;
    Ok(Json(ToolGrantsDto::build_with(&record, takes_effect)))
}

/// `DELETE {scope}/tools/grants` — withdraw one console grant
/// (`?namespace=paypal`) or all of them.
///
/// Deleting what was never granted is a no-op rather than a `404`: the caller's
/// intent ("this company should not grant this from the console") is already
/// satisfied. A seed grant is untouchable here by construction — the overlay is
/// the only thing removed, and the manifest is then re-folded from it.
async fn clear_grants(
    company: ScopedCompany,
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::server::graphql::auth::MaybePeer(peer): crate::server::graphql::auth::MaybePeer,
    axum::extract::Query(query): axum::extract::Query<ClearQuery>,
) -> Result<Json<ToolGrantsDto>, crate::server::Rejection> {
    require_admin(&headers, &state, &company.runtime, peer).await?;

    let write_lock = company_write_lock(company.id());
    let _lock = write_lock.lock().await;

    let mut record = load_record(&company).await?;
    let held = record.overlay_tool_grants.clone();

    // Recover version control's own list BEFORE touching anything, then rebuild
    // the manifest from it. Subtracting the withdrawn namespaces from the folded
    // list directly is the obvious version of this, and it is wrong: a
    // `?namespace=` naming a namespace the SEED grants — an operator tidying up,
    // or simply guessing — would strip a manifest grant this layer has no
    // authority over, and the company would lose the tool until its next rebuild
    // re-read `company.toml`. Recomputing from the seed makes that structurally
    // impossible rather than merely guarded against: the only inputs to the new
    // list are the seed and whatever survives in the override.
    let seed = seed_tool_allow(&record.manifest.tools.allow, held.as_ref());
    let requested = query
        .namespace
        .as_ref()
        .map(|namespace| namespace.trim().to_ascii_lowercase());

    record.overlay_tool_grants = held.and_then(|mut held| {
        match &requested {
            Some(namespace) => held.added.retain(|grant| grant != namespace),
            // A bare `DELETE` withdraws every console grant.
            None => held.added.clear(),
        }
        (!held.is_empty()).then_some(held)
    });
    record.manifest.tools.allow = effective_tool_allow(&seed, record.overlay_tool_grants.as_ref());

    save(&company, &record).await?;
    // Release the write lock before `apply_to_runtime` for the same reason
    // `add_grant` does (PR #1875 review finding): it may call
    // `rebuild_company`, which now serializes its own load-through-save on
    // this same lock, and this task still holding it would deadlock.
    drop(_lock);
    // A withdrawal needs the same treatment as a grant, and arguably needs it
    // more: a revocation the runtime has not picked up is a capability the
    // operator believes they removed and the agents still hold.
    let takes_effect = apply_to_runtime(&state, &company).await;
    Ok(Json(ToolGrantsDto::build_with(&record, takes_effect)))
}

fn refusal(message: &str) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, message.to_string()).into_response()
}

async fn load_record(company: &ScopedCompany) -> Result<CompanyRecord, crate::server::Rejection> {
    company
        .runtime
        .store()
        .load(company.id())
        .await?
        .ok_or_else(|| {
            ApiError(OpenCompanyError::CompanyNotFound(company.id().to_string()))
                .into_response()
                .into()
        })
}

async fn save(
    company: &ScopedCompany,
    record: &CompanyRecord,
) -> Result<(), crate::server::Rejection> {
    company
        .runtime
        .store()
        .save(record)
        .await
        .map_err(|e| ApiError(e).into_response().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::company::CompanyManifest;
    use crate::ports::CompanyStore;
    use crate::ports::types::CompanyId;
    use crate::runtime::RuntimeBuilder;
    use crate::server::router;
    use crate::store::FsCompanyStore;
    use crate::{AppConfig, AppState};

    /// A company whose `[tools].allow` is the catch-all plus one named grant.
    /// The catch-all is the point: it covers `shell`/`code`/`web` and confers
    /// none of the five this route deals in, which is the manifest shape #1796
    /// was reported against.
    const MANIFEST: &str = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [tools]\nallow = [\"*\", \"search\"]\n";

    fn home() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("oc-tool-grants-")
            .tempdir()
            .expect("tempdir")
    }

    async fn state(home: &std::path::Path) -> AppState {
        let manifest: CompanyManifest = toml::from_str(MANIFEST).unwrap();
        let store = FsCompanyStore::new(home.to_path_buf());
        let id = CompanyId::new("acme");
        store
            .save(&CompanyRecord {
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
        crate::server::test_support::seed_fixed_admin(&state, "acme").await;
        state
    }

    async fn call(
        state: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", crate::server::test_support::fixed_cookie("acme"))
            .header("content-type", "application/json");
        let request = match body {
            Some(value) => request.body(Body::from(value.to_string())).unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    const URI: &str = "/api/v1/company/tools/grants";

    /// The read surface before anything is granted: the manifest's list, an
    /// empty `added`, and the closed list of what this page may add. The
    /// console renders a control off `grantable`, so an empty one would be the
    /// dead end #1796 is about.
    #[tokio::test]
    async fn get_reports_the_manifest_grants_and_what_may_be_added() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "GET", URI, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["allow"], json!(["*", "search"]));
        assert_eq!(body["manifestAllow"], json!(["*", "search"]));
        assert_eq!(body["added"], json!([]));
        assert_eq!(
            body["grantable"],
            json!(["chargebee", "composio", "hosting", "paypal", "search"])
        );
        assert!(body["setBy"].is_null());
    }

    /// The whole point of the issue: granting `chargebee` from the console
    /// makes the company grant it, and the grant is attributed.
    #[tokio::test]
    async fn granting_a_namespace_widens_the_effective_allow_list() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) =
            call(&state, "PUT", URI, Some(json!({"namespace": "chargebee"}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["allow"], json!(["*", "search", "chargebee"]));
        assert_eq!(body["added"], json!(["chargebee"]));
        assert_eq!(body["manifestAllow"], json!(["*", "search"]));
        assert!(body["setBy"].is_string(), "a grant must be attributed");
        assert!(body["setAtMillis"].is_number());
    }

    /// The grant reaches the readers that decide whether an agent gets the
    /// tools — not just this route's own view of it. `grants_chargebee_explicit`
    /// over the stored manifest is exactly what `/billing/chargebee` and the
    /// harness tool wiring each ask, so asserting it here is asserting that
    /// "Connected" and "usable" have stopped disagreeing.
    #[tokio::test]
    async fn a_console_grant_is_visible_to_the_harness_grant_check() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, _) = call(&state, "PUT", URI, Some(json!({"namespace": "chargebee"}))).await;
        assert_eq!(status, StatusCode::OK);

        let store = FsCompanyStore::new(dir.path().to_path_buf());
        let record = store
            .load(&CompanyId::new("acme"))
            .await
            .unwrap()
            .expect("the company is stored");
        assert!(
            crate::company::grants_chargebee_explicit(&record.manifest.tools.allow),
            "the stored manifest must grant it: {:?}",
            record.manifest.tools.allow
        );
        assert!(
            crate::company::grants_chargebee_explicit(&record.effective_tool_allow()),
            "and so must the effective list"
        );
    }

    /// The catch-all does not confer these namespaces, and this route does not
    /// quietly change that: before the grant, `chargebee` is refused by the very
    /// check the harness makes, `*` notwithstanding.
    #[tokio::test]
    async fn the_catch_all_still_confers_nothing_before_the_grant() {
        let dir = home();
        let _state = state(dir.path()).await;
        let store = FsCompanyStore::new(dir.path().to_path_buf());
        let record = store
            .load(&CompanyId::new("acme"))
            .await
            .unwrap()
            .expect("the company is stored");
        assert!(record.manifest.tools.allow.iter().any(|g| g == "*"));
        assert!(!crate::company::grants_chargebee_explicit(
            &record.effective_tool_allow()
        ));
    }

    /// The closed list is a boundary, not a hint. `shell` has no connect page
    /// and no credential form, so a settings page that could confer it would be
    /// the general capability-widening surface the seed-wins rule forbids.
    #[tokio::test]
    async fn a_namespace_outside_the_closed_list_is_refused() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, _) = call(&state, "PUT", URI, Some(json!({"namespace": "shell"}))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (_, body) = call(&state, "GET", URI, None).await;
        assert_eq!(body["added"], json!([]), "nothing may be stored");
    }

    /// Even a namespace smuggled into the stored overlay confers nothing: the
    /// closed list is enforced again at resolution, so a row that reached the
    /// store under version skew cannot hand an agent a shell.
    #[tokio::test]
    async fn a_smuggled_namespace_confers_nothing_at_resolution() {
        let dir = home();
        let state = state(dir.path()).await;
        let store = FsCompanyStore::new(dir.path().to_path_buf());
        let id = CompanyId::new("acme");
        let mut record = store.load(&id).await.unwrap().unwrap();
        record.manifest.tools.allow = vec!["files".to_string()];
        record.overlay_tool_grants = Some(ToolGrantsOverride {
            added: vec!["shell".to_string()],
            set_by: Actor {
                kind: ActorKind::User,
                id: "someone".to_string(),
            },
            at_millis: 1,
        });
        store.save(&record).await.unwrap();

        let stored = store.load(&id).await.unwrap().unwrap();
        assert_eq!(
            stored.effective_tool_allow(),
            vec!["files".to_string()],
            "the stored `shell` must be dropped, not resolved"
        );
        let (_, body) = call(&state, "GET", URI, None).await;
        assert_eq!(body["allow"], json!(["files"]));
    }

    /// Granting what version control already grants stores nothing. Otherwise
    /// the console would claim credit for a seed grant and a later `DELETE`
    /// would look like it had revoked one — which this layer cannot do.
    #[tokio::test]
    async fn granting_a_seed_namespace_stores_no_override() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "PUT", URI, Some(json!({"namespace": "search"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["added"], json!([]));
        assert_eq!(body["allow"], json!(["*", "search"]));
        assert!(body["setBy"].is_null());
    }

    /// Two grants accumulate rather than replacing each other — an operator who
    /// wires PayPal after Chargebee has not thereby un-wired Chargebee.
    #[tokio::test]
    async fn grants_accumulate() {
        let dir = home();
        let state = state(dir.path()).await;
        call(&state, "PUT", URI, Some(json!({"namespace": "chargebee"}))).await;
        let (_, body) = call(&state, "PUT", URI, Some(json!({"namespace": "paypal"}))).await;
        assert_eq!(body["added"], json!(["chargebee", "paypal"]));
        assert_eq!(body["allow"], json!(["*", "search", "chargebee", "paypal"]));
    }

    /// Granting the same namespace twice is idempotent, not a duplicate entry —
    /// and the second call does not re-attribute the first.
    ///
    /// A retried `PUT` (a lost response, a second admin making sure) must not
    /// move `setBy`/`setAtMillis` onto whoever asked last. A capability widening
    /// that records the wrong person is barely better than one that records
    /// nobody, and the grant happened once.
    #[tokio::test]
    async fn granting_twice_is_idempotent_and_keeps_the_original_attribution() {
        let dir = home();
        let state = state(dir.path()).await;

        // Seeded rather than granted twice in a row on purpose. Two live calls
        // share one `now_millis()` at this speed and one signed-in admin, so
        // comparing their responses would pass whether or not the second call
        // re-stamped anything — a test that cannot fail. A distinct actor and an
        // obviously-old timestamp make the re-attribution visible.
        let store = FsCompanyStore::new(dir.path().to_path_buf());
        let id = CompanyId::new("acme");
        let mut record = store.load(&id).await.unwrap().unwrap();
        record.overlay_tool_grants = Some(ToolGrantsOverride {
            added: vec!["hosting".to_string()],
            set_by: Actor {
                kind: ActorKind::User,
                id: "the-operator-who-actually-granted-it".to_string(),
            },
            at_millis: 1_700_000_000_000,
        });
        record.manifest.tools.allow = record.effective_tool_allow();
        store.save(&record).await.unwrap();

        let (status, body) = call(&state, "PUT", URI, Some(json!({"namespace": "hosting"}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["added"], json!(["hosting"]), "duplicated the entry");
        assert_eq!(body["allow"], json!(["*", "search", "hosting"]));
        assert_eq!(body["setBy"], "the-operator-who-actually-granted-it");
        assert_eq!(body["setAtMillis"], 1_700_000_000_000u64);

        // And nothing was written, so the stored record says the same.
        let after = store.load(&id).await.unwrap().unwrap();
        let held = after.overlay_tool_grants.expect("still granted");
        assert_eq!(held.set_by.id, "the-operator-who-actually-granted-it");
        assert_eq!(held.at_millis, 1_700_000_000_000);
    }

    /// A single withdrawal leaves the other console grants alone, and actually
    /// removes the namespace from the folded manifest every reader consults —
    /// a revocation that only cleared the overlay would leave the tool wired.
    #[tokio::test]
    async fn one_namespace_can_be_withdrawn() {
        let dir = home();
        let state = state(dir.path()).await;
        call(&state, "PUT", URI, Some(json!({"namespace": "chargebee"}))).await;
        call(&state, "PUT", URI, Some(json!({"namespace": "paypal"}))).await;
        let (status, body) = call(
            &state,
            "DELETE",
            "/api/v1/company/tools/grants?namespace=paypal",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["added"], json!(["chargebee"]));
        assert_eq!(body["allow"], json!(["*", "search", "chargebee"]));

        let store = FsCompanyStore::new(dir.path().to_path_buf());
        let record = store.load(&CompanyId::new("acme")).await.unwrap().unwrap();
        assert!(
            !crate::company::grants_paypal_explicit(&record.manifest.tools.allow),
            "the withdrawal must reach the list the harness reads: {:?}",
            record.manifest.tools.allow
        );
    }

    /// **A `DELETE` naming a namespace the SEED grants must change nothing.**
    ///
    /// The obvious implementation subtracts the requested namespace from the
    /// folded `[tools].allow`, which quietly strips a manifest grant this layer
    /// has no authority over — the company then loses the tool until its next
    /// rebuild re-reads `company.toml`, and on a hosted tenant that is the next
    /// restart. `search` is in this fixture's manifest precisely so this can be
    /// asserted against a real seed grant rather than a hypothetical one.
    #[tokio::test]
    async fn withdrawing_a_seed_namespace_leaves_it_granted() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(
            &state,
            "DELETE",
            "/api/v1/company/tools/grants?namespace=search",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["allow"], json!(["*", "search"]), "{body}");
        assert_eq!(body["manifestAllow"], json!(["*", "search"]));

        let store = FsCompanyStore::new(dir.path().to_path_buf());
        let record = store.load(&CompanyId::new("acme")).await.unwrap().unwrap();
        assert!(
            crate::company::grants_search_explicit(&record.manifest.tools.allow),
            "the seed grant must survive: {:?}",
            record.manifest.tools.allow
        );
    }

    /// The same, with a console grant also standing: withdrawing the seed's
    /// namespace must leave BOTH the seed grant and the unrelated console grant
    /// exactly where they were.
    #[tokio::test]
    async fn withdrawing_a_seed_namespace_leaves_the_console_grants_alone() {
        let dir = home();
        let state = state(dir.path()).await;
        call(&state, "PUT", URI, Some(json!({"namespace": "chargebee"}))).await;
        let (_, body) = call(
            &state,
            "DELETE",
            "/api/v1/company/tools/grants?namespace=search",
            None,
        )
        .await;
        assert_eq!(body["allow"], json!(["*", "search", "chargebee"]), "{body}");
        assert_eq!(body["added"], json!(["chargebee"]));
    }

    /// A bare `DELETE` clears every console grant and restores the manifest's
    /// own list byte for byte — including the seed grants, which this layer
    /// never had the power to remove.
    #[tokio::test]
    async fn clearing_restores_the_manifest_list() {
        let dir = home();
        let state = state(dir.path()).await;
        call(&state, "PUT", URI, Some(json!({"namespace": "chargebee"}))).await;
        call(&state, "PUT", URI, Some(json!({"namespace": "composio"}))).await;
        let (status, body) = call(&state, "DELETE", URI, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["added"], json!([]));
        assert_eq!(body["allow"], json!(["*", "search"]));
        assert!(body["setBy"].is_null());
    }

    /// Clearing what was never granted is a no-op, not a 404: the caller's
    /// intent is already satisfied.
    #[tokio::test]
    async fn clearing_nothing_is_a_no_op() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "DELETE", URI, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["allow"], json!(["*", "search"]));
    }

    /// Widening what the company's agents can reach is an admin action. A
    /// signed-in non-admin reads the list and cannot move it.
    #[tokio::test]
    async fn a_non_admin_may_read_but_not_grant() {
        let dir = home();
        let state = state(dir.path()).await;
        crate::server::test_support::seed_fixed_member(&state, "acme").await;
        let request = Request::builder()
            .method("PUT")
            .uri(URI)
            .header("cookie", crate::server::test_support::member_cookie("acme"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"namespace": "chargebee"}).to_string()))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// **A grant must not claim "next turn" on a runtime that cannot deliver it.**
    ///
    /// This test used to assert the opposite, and was wrong for the reason
    /// Codex caught on #1832: the fixture boots on the echo brain — no
    /// inference credential, and `openhuman` is absent from the default build —
    /// so it is on a NON-harness cognition path, and `HarnessPool::ensure` (the
    /// only live grant refresh) never runs for it. `AppState::default` wires no
    /// rebuilder either.
    ///
    /// That is exactly the configuration where storing the record and reporting
    /// the harness timing tells an operator the integration now reaches their
    /// teammates while its belts stay as they were at boot — #1796 reproduced
    /// one layer inside its own fix. The response must say restart.
    #[tokio::test]
    async fn a_grant_on_a_non_harness_runtime_reports_restart_not_next_turn() {
        let dir = home();
        let state = state(dir.path()).await;
        let (status, body) = call(&state, "PUT", URI, Some(json!({"namespace": "hosting"}))).await;
        assert_eq!(status, StatusCode::OK);

        // The grant is stored and durable regardless: the timing is a statement
        // about reach, not about whether the write landed.
        assert_eq!(body["added"], json!(["hosting"]));
        assert_eq!(body["allow"], json!(["*", "search", "hosting"]));

        assert_eq!(
            body["takesEffect"], TAKES_EFFECT_RESTART,
            "an echo-brain company with no rebuilder cannot honour the next-turn promise"
        );
        assert_ne!(body["takesEffect"], TAKES_EFFECT);
    }

    /// A withdrawal reports the same way, and needs it more: a revocation the
    /// runtime has not picked up is a capability the operator believes they
    /// removed and the agents still hold.
    #[tokio::test]
    async fn a_withdrawal_on_a_non_harness_runtime_reports_restart_too() {
        let dir = home();
        let state = state(dir.path()).await;
        call(&state, "PUT", URI, Some(json!({"namespace": "hosting"}))).await;
        let (status, body) = call(&state, "DELETE", URI, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["added"], json!([]));
        assert_eq!(body["takesEffect"], TAKES_EFFECT_RESTART);
    }

    /// A plain read makes no claim about a write, so it keeps the live-refresh
    /// wording. Only a write that had to reach a runtime reports what happened.
    #[tokio::test]
    async fn the_read_route_reports_the_live_timing() {
        let dir = home();
        let state = state(dir.path()).await;
        let (_, body) = call(&state, "GET", URI, None).await;
        assert_eq!(body["takesEffect"], TAKES_EFFECT);
    }

    /// Storing nothing reaches no runtime, so it makes no claim about one:
    /// granting what version control already grants keeps the read wording and
    /// does not rebuild anything.
    #[tokio::test]
    async fn a_no_op_grant_makes_no_runtime_claim() {
        let dir = home();
        let state = state(dir.path()).await;
        let (_, body) = call(&state, "PUT", URI, Some(json!({"namespace": "search"}))).await;
        assert_eq!(body["added"], json!([]));
        assert_eq!(body["takesEffect"], TAKES_EFFECT);
    }

    // --- Lock-release-before-rebuild (PR #1875 review finding, CodeRabbit) --

    /// A rebuilder that always succeeds, so `apply_to_runtime` actually reaches
    /// `rebuild_company` instead of short-circuiting on `can_rebuild_in_place()
    /// == false` — the default `state()` fixture has none wired, which is why
    /// every other test above reads `TAKES_EFFECT_RESTART` rather than
    /// exercising the rebuild path at all.
    struct AlwaysRebuilds {
        home: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl crate::runtime::RuntimeRebuilder for AlwaysRebuilds {
        async fn rebuild(
            &self,
            _state: &AppState,
            request: crate::runtime::RebuildRequest,
        ) -> crate::Result<crate::company::runtime::CompanyRuntime> {
            RuntimeBuilder::new(self.home.clone(), request.manifest)
                .with_id(request.id)
                .with_handover(request.handover)
                .build()
                .await
        }
    }

    async fn state_with_rebuilder(home: &std::path::Path) -> AppState {
        state(home)
            .await
            .with_rebuilder(std::sync::Arc::new(AlwaysRebuilds {
                home: home.to_path_buf(),
            }))
    }

    /// `add_grant` drops `company_write_lock` before `apply_to_runtime` might
    /// call into `rebuild_company` — which now takes that same non-reentrant
    /// lock itself, so a task still holding it across the call would deadlock
    /// against its own rebuild. Nothing proved that until this test; proven
    /// the same way `rebuild_company_serializes_against_the_company_write_lock`
    /// (`src/runtime/rebuild.rs`) proves the equivalent property one layer
    /// down: hold the lock externally, drive the real request through the
    /// router, and demand it completes only once the lock is released.
    #[tokio::test]
    async fn add_grant_does_not_deadlock_against_its_own_rebuild() {
        let dir = home();
        let state = state_with_rebuilder(dir.path()).await;

        let lock = company_write_lock(&CompanyId::new("acme"));
        let guard = lock.lock().await;

        let state_for_task = state.clone();
        let mut task = tokio::spawn(async move {
            call(
                &state_for_task,
                "PUT",
                URI,
                Some(json!({"namespace": "hosting"})),
            )
            .await
        });

        // The request must be blocked behind the held lock — give it every
        // chance to (wrongly) race ahead before declaring it stuck.
        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "add_grant completed while company_write_lock was held elsewhere — it is not \
             serializing its save against a concurrent writer"
        );

        drop(guard);
        let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect(
                "add_grant never resumed after the lock was released — it deadlocked against \
                 its own rebuild_company call",
            )
            .expect("task panicked");
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// Same property, for `clear_grants` (PR #1875 review finding, CodeRabbit).
    #[tokio::test]
    async fn clear_grants_does_not_deadlock_against_its_own_rebuild() {
        let dir = home();
        let state = state_with_rebuilder(dir.path()).await;
        // Seeded before the lock is held externally, so the withdrawal below
        // actually changes the manifest and reaches `apply_to_runtime`.
        call(&state, "PUT", URI, Some(json!({"namespace": "hosting"}))).await;

        let lock = company_write_lock(&CompanyId::new("acme"));
        let guard = lock.lock().await;

        let state_for_task = state.clone();
        let mut task =
            tokio::spawn(async move { call(&state_for_task, "DELETE", URI, None).await });

        let raced_ahead = tokio::time::timeout(std::time::Duration::from_millis(200), &mut task)
            .await
            .is_ok();
        assert!(
            !raced_ahead,
            "clear_grants completed while company_write_lock was held elsewhere — it is not \
             serializing its save against a concurrent writer"
        );

        drop(guard);
        let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect(
                "clear_grants never resumed after the lock was released — it deadlocked \
                 against its own rebuild_company call",
            )
            .expect("task panicked");
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}
