//! Inbound tiny.place A2A surface: JSON-RPC `tasks/send`, discovery records, and
//! the human-readable skill catalog.
//!
//! This whole module is gated behind the `tinyplace` feature — with the feature
//! off no A2A routes are mounted and the default build links no crypto. When on,
//! [`router`] serves:
//!
//! ```text
//! POST /a2a/{handle}                                    -> a2a_task
//! GET  /a2a/{handle}/skill.md                           -> skill_md
//! GET  /a2a/{handle}                                    -> agent_card
//! GET  /.well-known/agent-card.json                     -> well_known_sole
//! GET  /companies/{handle}/.well-known/agent-card.json  -> well_known_platform
//! ```
//!
//! The `tasks/send` handler enforces the tiny.place trust boundary in a fixed
//! order: resolve a **discoverable** company, verify the SIWX `Authorization`
//! (skew + single-use replay protection via the host-global
//! [`NonceCache`](crate::economy::NonceCache)) before anything reaches cognition,
//! answer a `402` challenge for a priced skill lacking a valid
//! [`X402Authorization`](crate::economy::X402Authorization), sanitize the
//! counterparty payload (a minimal promptguard pass), and only then append an
//! [`A2aTaskReceived`](crate::ports::types::CompanyEvent::A2aTaskReceived) event
//! and run one cycle. A paying customer runs under the same approval gates as any
//! other stimulus — there is no fence bypass.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::AppState;
use crate::company::CompanyManifest;
use crate::company::runtime::CompanyRuntime;
use crate::economy::client::{JsonRpcRequest, JsonRpcResponse, now_secs, sha256_hex};
use crate::economy::signer::signer_for;
use crate::economy::x402::{self, X402Authorization};
use crate::economy::{build_agent_card, render_skill_md, siwx};
use crate::error::OpenCompanyError;
use crate::ports::now_millis;
use crate::ports::types::{AgentCard, CardPayment, CompanyEvent, LedgerEntry};
use crate::server::error::ApiError;

/// Builds the tiny.place A2A route fragment, merged into the main router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/a2a/{handle}", post(a2a_task).get(agent_card))
        .route("/a2a/{handle}/skill.md", get(skill_md))
        .route("/.well-known/agent-card.json", get(well_known_sole))
        .route(
            "/companies/{handle}/.well-known/agent-card.json",
            get(well_known_platform),
        )
}

// ---------------------------------------------------------------------------
// Company resolution
// ---------------------------------------------------------------------------

/// Resolves a `@handle` to a running, **discoverable** company.
///
/// Scans the registry for a company whose manifest sets `[place].discoverable`
/// and whose `[company].handle` matches, falling back to the sole registered
/// company in prosumer mode when it too is discoverable. A miss is a 404. The
/// linear scan is fine at prosumer / small-platform scale; a handle index is a
/// documented follow-up.
async fn resolve_company(state: &AppState, handle: &str) -> Result<Arc<CompanyRuntime>, ApiError> {
    for id in state.registry().list() {
        let Some(runtime) = state.registry().get(&id) else {
            continue;
        };
        if let Some(record) = runtime.store.load(&id).await?
            && record.manifest.place.discoverable
            && record.manifest.company.handle.as_deref() == Some(handle)
        {
            return Ok(runtime);
        }
    }

    // Prosumer fallback: a lone discoverable company answers any handle.
    if let Some(runtime) = state.registry().sole()
        && let Some(record) = runtime.store.load(runtime.id()).await?
        && record.manifest.place.discoverable
    {
        return Ok(runtime);
    }

    Err(ApiError(OpenCompanyError::CompanyNotFound(
        handle.to_string(),
    )))
}

/// Loads a resolved company's manifest, erroring 404 when the record is missing.
async fn load_manifest(runtime: &CompanyRuntime) -> Result<CompanyManifest, ApiError> {
    runtime
        .store
        .load(runtime.id())
        .await?
        .map(|record| record.manifest)
        .ok_or_else(|| ApiError(OpenCompanyError::CompanyNotFound(runtime.id().to_string())))
}

/// Builds a resolved company's Agent Card against the host base URL.
async fn card_for(state: &AppState, runtime: &CompanyRuntime) -> Result<AgentCard, ApiError> {
    let manifest = load_manifest(runtime).await?;
    Ok(build_agent_card(&manifest, &state.config().host_base_url()))
}

// ---------------------------------------------------------------------------
// Read-only discovery routes (no SIWX)
// ---------------------------------------------------------------------------

/// `GET /a2a/{handle}` — the company's Agent Card (a directory-record convenience).
async fn agent_card(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(handle): axum::extract::Path<String>,
) -> Result<Json<AgentCard>, ApiError> {
    let runtime = resolve_company(&state, &handle).await?;
    Ok(Json(card_for(&state, &runtime).await?))
}

/// `GET /.well-known/agent-card.json` — the sole company's card (prosumer mode).
async fn well_known_sole(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<AgentCard>, ApiError> {
    let runtime = state.registry().sole().ok_or_else(|| {
        ApiError(OpenCompanyError::CompanyNotFound(
            "single-company".to_string(),
        ))
    })?;
    // Discoverability is opt-in: an undiscoverable sole company is not published
    // through the well-known card either.
    let discoverable = runtime
        .store
        .load(runtime.id())
        .await?
        .map(|record| record.manifest.place.discoverable)
        .unwrap_or(false);
    if !discoverable {
        return Err(ApiError(OpenCompanyError::CompanyNotFound(
            "single-company".to_string(),
        )));
    }
    Ok(Json(card_for(&state, &runtime).await?))
}

/// `GET /companies/{handle}/.well-known/agent-card.json` — a named company's card.
async fn well_known_platform(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(handle): axum::extract::Path<String>,
) -> Result<Json<AgentCard>, ApiError> {
    let runtime = resolve_company(&state, &handle).await?;
    Ok(Json(card_for(&state, &runtime).await?))
}

/// `GET /a2a/{handle}/skill.md` — the human- and agent-readable skill catalog.
async fn skill_md(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(handle): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    let runtime = resolve_company(&state, &handle).await?;
    let card = card_for(&state, &runtime).await?;
    let body = render_skill_md(&card);
    Ok(([(CONTENT_TYPE, "text/markdown; charset=utf-8")], body).into_response())
}

// ---------------------------------------------------------------------------
// The inbound task route
// ---------------------------------------------------------------------------

/// `POST /a2a/{handle}` — a SIWX-authenticated JSON-RPC `tasks/send`.
async fn a2a_task(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(handle): axum::extract::Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. Resolve a discoverable company; 404 otherwise.
    let runtime = match resolve_company(&state, &handle).await {
        Ok(runtime) => runtime,
        Err(err) => return err.into_response(),
    };

    // A company with no economy wired is not reachable for commerce → 503.
    if !runtime.has_economy() {
        return ApiError(OpenCompanyError::tinyplace(
            "unreachable",
            format!("@{handle} is not reachable for A2A tasks"),
        ))
        .into_response();
    }

    // Lifecycle: a paused/archived company rejects work → 409.
    if let Err(err) = runtime.ensure_running().await {
        return ApiError(err).into_response();
    }

    // 2. SIWX — verified before anything reaches cognition. A bad or missing
    // header is a 401; nothing is logged from the request until it verifies.
    let path = format!("/a2a/{handle}");
    let body_hash = sha256_hex(&body);
    let auth_header = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let from = match siwx::verify(
        auth_header,
        "POST",
        &path,
        &body_hash,
        now_secs(),
        state.nonce(),
    ) {
        Ok(agent_id) => agent_id,
        Err(err) => return unauthorized(&err),
    };

    // 3. Parse the JSON-RPC `tasks/send` envelope.
    let rpc: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(rpc) => rpc,
        Err(err) => {
            return ApiError(OpenCompanyError::InvalidRequest(format!(
                "body is not a JSON-RPC request: {err}"
            )))
            .into_response();
        }
    };
    if rpc.method != "tasks/send" {
        return ApiError(OpenCompanyError::InvalidRequest(format!(
            "unsupported method `{}`; only `tasks/send` is served",
            rpc.method
        )))
        .into_response();
    }
    let skill = rpc
        .params
        .get("skill")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Pricing comes from the company's own Agent Card.
    let card = match card_for(&state, &runtime).await {
        Ok(card) => card,
        Err(err) => return err.into_response(),
    };

    // 4. If the requested skill is priced above zero, require a valid x402
    // authorization. A `0.00` (or unparsable) price is served for free.
    if let Some(pay) = card.payment_requirements.iter().find(|p| {
        p.skill_id == skill
            && p.price
                .trim()
                .parse::<f64>()
                .map(|v| v > 0.0)
                .unwrap_or(false)
    }) {
        match extract_payment(&rpc.params) {
            None => return payment_required(&state, &runtime, pay).await,
            Some(auth) => {
                if x402::verify(&auth).is_err() {
                    return ApiError(OpenCompanyError::InvalidRequest(
                        "x402 payment authorization did not verify".into(),
                    ))
                    .into_response();
                }
                // Bind the payment to THIS company: the payer must have signed a
                // `recipient` equal to our own agent id. Without this a
                // counterparty could self-sign an authorization paying anyone
                // else and still obtain priced work.
                let our_id = match signer_for(state.home(), runtime.id()).await {
                    Ok(signer) => signer.agent_id(),
                    Err(err) => return ApiError(err).into_response(),
                };
                if auth.recipient != our_id {
                    return payment_required(&state, &runtime, pay).await;
                }
                let paid = auth.amount.trim().parse::<f64>().unwrap_or(0.0);
                let price = pay.price.trim().parse::<f64>().unwrap_or(f64::INFINITY);
                if paid < price {
                    // Underpaid: re-challenge for the correct amount.
                    return payment_required(&state, &runtime, pay).await;
                }
                // Journal the inbound receipt before doing the work.
                let entry = LedgerEntry {
                    at_millis: now_millis(),
                    kind: "x402.in".to_string(),
                    amount_usd: paid,
                    memo: format!("a2a `{skill}` from {from}"),
                };
                if let Err(err) = runtime.store.append_ledger(runtime.id(), entry).await {
                    return ApiError(err).into_response();
                }
            }
        }
    }

    // 5. Promptguard: sanitize the counterparty payload before it becomes an
    // event. Deliberately minimal — a control-character strip seam, not a full
    // model-based guard.
    let task = sanitize_value(rpc.params.clone());

    // 6. Append the event and run one cycle (run_cycle persists the event).
    let report = match runtime
        .run_cycle(vec![CompanyEvent::A2aTaskReceived {
            from: from.clone(),
            task,
        }])
        .await
    {
        Ok(report) => report,
        Err(err) => return ApiError(err).into_response(),
    };

    let result = json!({
        "cycleId": report.cycle_id,
        "responses": report.responses,
    });
    (StatusCode::OK, Json(JsonRpcResponse::ok(rpc.id, result))).into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Renders a SIWX failure as a `401` in the api.md error envelope.
fn unauthorized(err: &OpenCompanyError) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": err.to_string(), "code": err.code() })),
    )
        .into_response()
}

/// Builds the `402` challenge naming the price and the company's own address.
async fn payment_required(
    state: &AppState,
    runtime: &CompanyRuntime,
    pay: &CardPayment,
) -> Response {
    let recipient = match signer_for(state.home(), runtime.id()).await {
        Ok(signer) => signer.agent_id(),
        Err(err) => return ApiError(err).into_response(),
    };
    let challenge = json!({
        "amount": pay.price,
        "recipient": recipient,
        "asset": pay.asset,
        "network": pay.network,
    });
    (StatusCode::PAYMENT_REQUIRED, Json(challenge)).into_response()
}

/// Extracts an [`X402Authorization`] from a `payment` param, if present and valid.
fn extract_payment(params: &Value) -> Option<X402Authorization> {
    let payment = params.get("payment")?;
    serde_json::from_value(payment.clone()).ok()
}

/// Strips control characters (keeping ordinary whitespace) from counterparty
/// text so an injected escape/marker never reaches the brain verbatim.
fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
        .collect()
}

/// Recursively sanitizes every string in a JSON value.
fn sanitize_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(sanitize_text(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, sanitize_value(v)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::AppConfig;
    use crate::company::CompanyManifest;
    use crate::economy::signer::LocalSigner;
    use crate::economy::x402::X402Challenge;
    use crate::economy::{MockTinyplaceClient, TinyplaceEconomy};
    use crate::ports::types::{CompanyId, EventSeq};
    use crate::ports::{AgentEconomy, CompanyStore};
    use crate::runtime::RuntimeBuilder;
    use crate::store::FsCompanyStore;

    const DISCOVERABLE_TOML: &str = r#"
        [company]
        name = "Acme SEO"
        output = "SEO audits"
        handle = "acme"

        [place]
        discoverable = true
        skills = [
            { id = "seo.audit", price_usd = "25.00", description = "Full audit" },
            { id = "seo.free", price_usd = "0.00" },
        ]
    "#;

    /// Builds an `AppState` with one discoverable company wired to a mock
    /// economy, rooted at `home`, and returns the client-side signer to sign
    /// inbound requests with.
    async fn seeded_state(home: &std::path::Path) -> (AppState, Arc<LocalSigner>) {
        let manifest: CompanyManifest = toml::from_str(DISCOVERABLE_TOML).unwrap();
        let id = CompanyId::new("acme");
        let store: Arc<dyn CompanyStore> = Arc::new(FsCompanyStore::new(home.to_path_buf()));
        let signer = Arc::new(LocalSigner::generate());
        let mock = Arc::new(MockTinyplaceClient::new());
        let economy: Arc<dyn AgentEconomy> = Arc::new(
            TinyplaceEconomy::new(mock, signer.clone(), store.clone(), id.clone(), None)
                .going_public(true),
        );
        let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
            .with_id(id)
            .with_economy(economy)
            .build()
            .await
            .unwrap();

        let state = AppState::new(AppConfig::default()).with_home(home.to_path_buf());
        state
            .registry()
            .insert(runtime.id().clone(), Arc::new(runtime));

        // The counterparty (client) signs with its own identity.
        let client_signer = Arc::new(LocalSigner::generate());
        (state, client_signer)
    }

    /// Signs a POST body for `/a2a/{handle}` and returns the SIWX header value.
    fn siwx_header(signer: &LocalSigner, handle: &str, body: &[u8], ts: i64) -> String {
        let hash = sha256_hex(body);
        let header = siwx::build_header(
            signer,
            &siwx::SiwxPayload {
                method: "POST",
                path: &format!("/a2a/{handle}"),
                timestamp: ts,
                body_hash: &hash,
            },
        );
        siwx::header_value(&header)
    }

    fn task_body(skill: &str) -> Vec<u8> {
        serde_json::to_vec(&JsonRpcRequest::new(
            "tasks/send",
            json!({ "skill": skill, "input": { "site": "x.com" } }),
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn siwx_invalid_inbound_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _client) = seeded_state(dir.path()).await;
        let app = router().with_state(state);

        let body = task_body("seo.free");
        // No Authorization header at all.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a/acme")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn priced_skill_without_payment_returns_402() {
        let dir = tempfile::tempdir().unwrap();
        let (state, client) = seeded_state(dir.path()).await;
        // Our address is the on-disk signer for the company id.
        let our_id = signer_for(dir.path(), &CompanyId::new("acme"))
            .await
            .unwrap()
            .agent_id();
        let app = router().with_state(state);

        let body = task_body("seo.audit");
        let header = siwx_header(&client, "acme", &body, now_secs());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a/acme")
                    .header(AUTHORIZATION, header)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let challenge: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(challenge["amount"], "25.00");
        assert_eq!(challenge["recipient"], our_id);
        assert_eq!(challenge["asset"], "USDC");
        assert_eq!(challenge["network"], "solana");
    }

    #[tokio::test]
    async fn valid_signed_free_task_routes_to_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let (state, client) = seeded_state(dir.path()).await;
        let runtime = state.registry().sole().unwrap();
        let app = router().with_state(state);

        let body = task_body("seo.free");
        let header = siwx_header(&client, "acme", &body, now_secs());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a/acme")
                    .header(AUTHORIZATION, header)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["jsonrpc"], "2.0");
        assert!(envelope["result"]["cycleId"].is_string());

        // The A2aTaskReceived event was persisted by the cycle.
        let stored = runtime
            .events
            .read_from(runtime.id(), EventSeq::new(0), 10)
            .await
            .unwrap();
        assert!(stored.iter().any(|e| matches!(
            &e.event,
            CompanyEvent::A2aTaskReceived { from, .. } if from == &client.agent_id()
        )));
    }

    #[tokio::test]
    async fn paid_skill_with_valid_x402_routes_and_journals() {
        let dir = tempfile::tempdir().unwrap();
        let (state, client) = seeded_state(dir.path()).await;
        let runtime = state.registry().sole().unwrap();
        let our_id = signer_for(dir.path(), &CompanyId::new("acme"))
            .await
            .unwrap()
            .agent_id();
        let app = router().with_state(state);

        // Build a valid x402 authorization paying the 25.00 seo.audit price.
        let challenge = X402Challenge {
            amount: "25.00".into(),
            recipient: our_id,
            asset: "USDC".into(),
            network: "solana".into(),
        };
        let auth = x402::authorize(&client, &challenge, now_secs());
        let rpc = JsonRpcRequest::new(
            "tasks/send",
            json!({ "skill": "seo.audit", "input": {}, "payment": auth }),
        );
        let body = serde_json::to_vec(&rpc).unwrap();
        let header = siwx_header(&client, "acme", &body, now_secs());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a/acme")
                    .header(AUTHORIZATION, header)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        // The inbound receipt was journaled as x402.in.
        let record = runtime.store.load(runtime.id()).await.unwrap().unwrap();
        let inflow = record
            .ledger
            .iter()
            .find(|e| e.kind == "x402.in")
            .expect("x402.in row");
        assert_eq!(inflow.amount_usd, 25.0);
    }

    #[tokio::test]
    async fn paid_skill_with_wrong_recipient_is_rechallenged() {
        let dir = tempfile::tempdir().unwrap();
        let (state, client) = seeded_state(dir.path()).await;
        let app = router().with_state(state);

        // A well-formed, correctly-signed authorization that pays SOMEONE ELSE
        // (a self-dealing payer) must not buy priced work from this company.
        let challenge = X402Challenge {
            amount: "25.00".into(),
            recipient: client.agent_id(), // not our company's agent id
            asset: "USDC".into(),
            network: "solana".into(),
        };
        let auth = x402::authorize(&client, &challenge, now_secs());
        let rpc = JsonRpcRequest::new(
            "tasks/send",
            json!({ "skill": "seo.audit", "input": {}, "payment": auth }),
        );
        let body = serde_json::to_vec(&rpc).unwrap();
        let header = siwx_header(&client, "acme", &body, now_secs());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a/acme")
                    .header(AUTHORIZATION, header)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Re-challenged with a 402, not served for free.
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn replayed_signature_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (state, client) = seeded_state(dir.path()).await;
        let app = router().with_state(state);

        let body = task_body("seo.free");
        let header = siwx_header(&client, "acme", &body, now_secs());

        let build = || {
            Request::builder()
                .method("POST")
                .uri("/a2a/acme")
                .header(AUTHORIZATION, header.clone())
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(body.clone()))
                .unwrap()
        };

        let first = app.clone().oneshot(build()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        // The identical signature is rejected on replay.
        let second = app.oneshot(build()).await.unwrap();
        assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn promptguard_sanitizes_control_chars_before_event() {
        let dir = tempfile::tempdir().unwrap();
        let (state, client) = seeded_state(dir.path()).await;
        let runtime = state.registry().sole().unwrap();
        let app = router().with_state(state);

        // A bell (0x07) and ESC (0x1b) must be stripped; newline survives.
        let rpc = JsonRpcRequest::new(
            "tasks/send",
            json!({ "skill": "seo.free", "note": "hi\u{0007}there\u{001b}\nok" }),
        );
        let body = serde_json::to_vec(&rpc).unwrap();
        let header = siwx_header(&client, "acme", &body, now_secs());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a/acme")
                    .header(AUTHORIZATION, header)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let stored = runtime
            .events
            .read_from(runtime.id(), EventSeq::new(0), 10)
            .await
            .unwrap();
        let task = stored
            .iter()
            .find_map(|e| match &e.event {
                CompanyEvent::A2aTaskReceived { task, .. } => Some(task.clone()),
                _ => None,
            })
            .expect("a2a event");
        let note = task["note"].as_str().unwrap();
        assert_eq!(note, "hithere\nok");
    }

    #[tokio::test]
    async fn well_known_and_skill_md_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _client) = seeded_state(dir.path()).await;
        let app = router().with_state(state);

        // The platform well-known returns the card with the a2a endpoint.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/companies/acme/.well-known/agent-card.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let card: AgentCard = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(card.endpoint, "http://127.0.0.1:8080/a2a/acme");
        assert!(card.skills.contains(&"seo.audit".to_string()));

        // skill.md lists each priced skill line.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/a2a/acme/skill.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/markdown; charset=utf-8")
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let md = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(md.contains("`seo.audit` — 25.00 USDC (solana)"));
    }
}
