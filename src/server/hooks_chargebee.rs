//! The inbound Chargebee webhook: `POST /hooks/{company}/chargebee` (issue #788).
//!
//! Chargebee posts here when a payment succeeds or fails. A verified delivery
//! raises a [`CompanyEvent::WebhookReceived`] on the `chargebee` channel, which
//! drives one cycle — so the operator hears "Alan paid the $100 invoice" in
//! chat **without having asked**. That push is the whole point of this route.
//!
//! # Why this does not persist invoice state
//!
//! Issue #788's TC-03 describes the webhook updating stored state that the agent
//! then reads to answer "has Alan paid?". It deliberately does not do that.
//! `chargebee_get_invoice` already answers that question **live from
//! Chargebee**, and does it strictly better: stored state goes stale the moment
//! a delivery is dropped, retried, or replayed out of order, and then the agent
//! confidently reports a payment status that Chargebee disagrees with. A cache
//! that can silently diverge from the system of record is worse than no cache
//! when the subject is money.
//!
//! So the split is: **pull** stays live (`chargebee_get_invoice`), and this
//! route owns **push** — the thing a live read genuinely cannot do.
//!
//! # Verification comes before parsing
//!
//! Chargebee protects a webhook URL with HTTP Basic auth, configured beside the
//! URL in its dashboard. The handler compares that header, constant-time,
//! against the company's stored secret **before it parses anything**: an
//! unverifiable POST is dropped with `401` and never becomes an event. Same
//! order, and the same reason, as the Telegram hook next door.
//!
//! # The event names are Chargebee's, not the issue's
//!
//! #788 calls them `invoice_paid` and `invoice_payment_failed`. Chargebee has no
//! such events — the real ones are `payment_succeeded` and `payment_failed`
//! (plus `invoice_generated`). The names here are the ones an operator will
//! actually find in the Chargebee dashboard's webhook configuration.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::AppState;
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::CompanyEvent;
use crate::server::ops::resolve;

pub use crate::company::billing::WEBHOOK_SECRET_KEY;

/// The channel a verified delivery is raised on.
pub const CHANNEL: &str = "chargebee";

/// The Chargebee events this route acts on.
///
/// An unlisted event is acknowledged and ignored rather than refused: Chargebee
/// sends whatever the dashboard subscribes to, an operator will over-subscribe,
/// and answering non-2xx would make Chargebee retry — then disable the endpoint
/// — over an event we simply had no interest in.
const ACTED_ON: &[&str] = &["payment_succeeded", "payment_failed", "invoice_generated"];

/// The most body this route will buffer, well above any real Chargebee event.
///
/// The second of two bounds, and the weaker one. [`VerifiedDelivery`] means an
/// unauthenticated caller's body is never read at all; this caps what a caller
/// who DID authenticate can make the host allocate. A Chargebee event is a few
/// KiB — an invoice with a long line-item list is the largest realistic case and
/// nowhere near this — so the 2 MiB axum defaults to is simply more room than
/// the endpoint has any use for.
const MAX_EVENT_BYTES: usize = 256 * 1024;

/// Builds the Chargebee webhook route fragment.
pub fn router() -> Router<AppState> {
    Router::new().route(
        "/hooks/{company}/chargebee",
        post(chargebee_hook).layer(axum::extract::DefaultBodyLimit::max(MAX_EVENT_BYTES)),
    )
}

/// A `401` drop for an unverifiable delivery.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "invalid webhook credentials", "code": "unauthorized" })),
    )
        .into_response()
}

/// Length-checked, branch-independent byte comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Decodes an `Authorization: Basic <base64>` header into `user:pass`.
///
/// Hand-rolled because `base64` is behind the `mcp` feature and this route ships
/// in every build. Decoding is 20 lines; taking a feature dependency for it
/// would make an always-on route conditional on an unrelated one.
fn decode_basic(header: &str) -> Option<String> {
    let encoded = header.strip_prefix("Basic ")?.trim();
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    // Structure first. An earlier version decoded until it met a `=` or an
    // unknown byte, so `QQ` and `QQ=garbage` both yielded a prefix that then
    // went into a credential comparison — a decoder that accepts more than it
    // should is a poor thing to put in front of an auth check, even when the
    // comparison itself would fail.
    let bytes = encoded.as_bytes();
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    let padding = bytes.iter().rev().take_while(|b| **b == b'=').count();
    if padding > 2 {
        return None;
    }
    let body = &bytes[..bytes.len() - padding];
    if body.iter().any(|b| !ALPHABET.contains(b)) {
        return None;
    }

    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out: Vec<u8> = Vec::new();
    for byte in body {
        let value = ALPHABET.iter().position(|c| c == byte)? as u32;
        bits = (bits << 6) | value;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    // Leftover bits must be zero in canonical base64; anything else means the
    // input was not produced by an encoder.
    if nbits > 0 && (bits & ((1 << nbits) - 1)) != 0 {
        return None;
    }
    String::from_utf8(out).ok()
}

/// A delivery whose credential has already been verified.
///
/// This is an **extractor**, not a check inside the handler, and the difference
/// is the point: axum runs every `FromRequestParts` extractor before the one
/// `FromRequest` extractor that consumes the body, so an unverifiable POST is
/// rejected while its body is still on the socket. With the check inside the
/// handler, `Bytes` had already buffered whatever an unauthenticated caller
/// chose to send — bounded by the route's `DefaultBodyLimit`, but bounded is
/// not the same as never read.
///
/// Carrying the runtime in the type also means the handler cannot forget: there
/// is no path to the body that does not go through a verified credential.
struct VerifiedDelivery(Arc<CompanyRuntime>);

impl axum::extract::FromRequestParts<AppState> for VerifiedDelivery {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let Path(company) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let runtime = resolve(state, &company).map_err(IntoResponse::into_response)?;
        verify(&runtime, &parts.headers)
            .await
            .map_err(IntoResponse::into_response)?;
        Ok(Self(runtime))
    }
}

/// Compares the delivery's HTTP Basic credential against the company's stored
/// one, in constant time.
///
/// A stored credential must exist to verify against. An empty stored value
/// counts as "not configured" — reject rather than accept anything.
async fn verify(
    runtime: &Arc<CompanyRuntime>,
    headers: &HeaderMap,
) -> std::result::Result<(), crate::server::Rejection> {
    let expected = match runtime
        .secrets()
        .get(runtime.id(), WEBHOOK_SECRET_KEY)
        .await
    {
        Ok(Some(secret)) if !secret.expose().is_empty() => secret.expose().to_string(),
        Ok(_) => return Err(unauthorized().into()),
        Err(err) => return Err(crate::server::error::ApiError(err).into_response().into()),
    };

    let Some(provided) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(decode_basic)
    else {
        return Err(unauthorized().into());
    };
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(unauthorized().into());
    }
    Ok(())
}

/// `POST /hooks/{company}/chargebee`.
///
/// `VerifiedDelivery` comes first deliberately — it is the extractor that
/// authenticates, and `raw` is only read once it has succeeded.
async fn chargebee_hook(VerifiedDelivery(runtime): VerifiedDelivery, raw: Bytes) -> Response {
    handle(runtime, &raw).await
}

/// Raises one cycle for an event worth telling the operator about.
///
/// The credential is already verified — see [`VerifiedDelivery`].
async fn handle(runtime: Arc<CompanyRuntime>, raw: &[u8]) -> Response {
    let Ok(event) = serde_json::from_slice::<Value>(raw) else {
        // Malformed body from a caller that DID authenticate: accept it so
        // Chargebee stops retrying, and say so in the log rather than silently.
        tracing::warn!(company = %runtime.id().as_ref(), "[chargebee] webhook body was not JSON");
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "ignored": "unparseable"})),
        )
            .into_response();
    };

    let event_type = event
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !ACTED_ON.contains(&event_type.as_str()) {
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "ignored": event_type})),
        )
            .into_response();
    }

    let summary = summarize(&event_type, &event);
    tracing::info!(company = %runtime.id().as_ref(), %event_type, "[chargebee] webhook");

    // Drive one cycle so the company *says* something. A paused or archived
    // company acknowledges without running — Chargebee must not retry because
    // the operator happened to have the company stopped.
    if runtime.ensure_running().await.is_ok() {
        let event = CompanyEvent::WebhookReceived {
            channel: CHANNEL.to_string(),
            // The summary, not the raw event: `body` reaches the brain, and a
            // whole Chargebee payload spends a great deal of context to say
            // "Alan paid". The original is still in the log line above.
            body: json!({"event_type": event_type, "summary": summary}),
        };
        if let Err(err) = runtime.run_cycle(vec![event]).await {
            tracing::warn!(company = %runtime.id(), "chargebee cycle failed: {err}");
        }
    }

    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

/// Renders the event as the sentence the agent is asked to relay.
///
/// A summary rather than the raw payload: a Chargebee event carries the whole
/// invoice, customer, transaction and card objects, and handing that to a model
/// spends a large amount of context to say "Alan paid". Amounts stay in minor
/// units with the currency beside them — this text reaches a model, and a bare
/// `10000` with no unit is exactly how a $100 payment gets reported as $10,000.
fn summarize(event_type: &str, event: &Value) -> String {
    let content = event.get("content").cloned().unwrap_or(Value::Null);
    let invoice = content.get("invoice");
    let field = |obj: Option<&Value>, key: &str| -> Option<String> {
        obj?.get(key).and_then(Value::as_str).map(str::to_string)
    };
    let id = field(invoice, "id").unwrap_or_else(|| "(unknown)".to_string());
    let currency = field(invoice, "currency_code").unwrap_or_default();
    let total = invoice
        .and_then(|i| i.get("total"))
        .and_then(Value::as_i64)
        .map(|t| format!("{t} {currency} (minor units)"))
        .unwrap_or_else(|| "an unknown amount".to_string());
    // The customer id, never their email. This string is persisted in the
    // company journal and replayed into model prompts, so a counterparty's
    // address would outlive the notification it was needed for. The id is
    // sufficient to look them up in Chargebee.
    let who = field(content.get("customer"), "id").unwrap_or_else(|| "the customer".to_string());

    match event_type {
        "payment_succeeded" => format!(
            "Chargebee: invoice {id} for {total} was PAID by {who}. Tell the operator, briefly."
        ),
        "payment_failed" => format!(
            "Chargebee: a payment FAILED for invoice {id} ({total}) from {who}. Tell the operator, \
             briefly, and say the invoice is still outstanding."
        ),
        _ => format!("Chargebee: invoice {id} for {total} was generated for {who}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- End-to-end, through the real router ------------------------------
    //
    // The unit tests below cover `decode_basic` and `summarize`. These drive the
    // whole route, because the thing worth protecting is not either function on
    // its own — it is that an unverifiable POST cannot reach the parser, the
    // event filter, or a company cycle. This is the one surface here that
    // accepts input from outside the host.

    /// Every event the company's brain was actually driven with.
    ///
    /// The route answers `200` whether or not it raised anything, so the HTTP
    /// response cannot distinguish a working webhook from a handler that
    /// acknowledges and does nothing. This is the seam that can: it sits where
    /// the cycle actually lands.
    type Delivered = Arc<std::sync::Mutex<Vec<CompanyEvent>>>;

    /// A brain that records what it was asked to run, then behaves as the
    /// default one does.
    ///
    /// Delegating to [`EchoBrain`](crate::brain::EchoBrain) rather than
    /// returning a hand-built `CycleResult` keeps the cycle on the path it takes
    /// in these tests already — the recorder observes, it does not substitute.
    struct RecordingBrain(Delivered);

    #[async_trait::async_trait]
    impl crate::ports::brain::Brain for RecordingBrain {
        async fn run_cycle(
            &self,
            req: crate::ports::types::CycleRequest,
            host: &dyn crate::ports::brain::CycleHost,
        ) -> crate::Result<crate::ports::types::CycleResult> {
            self.0
                .lock()
                .expect("lock")
                .extend(req.events.iter().cloned());
            crate::brain::EchoBrain::new().run_cycle(req, host).await
        }
    }

    /// A host with one company, and the webhook credential `credential` stored
    /// when it is `Some`.
    async fn state_with(
        home: &std::path::Path,
        credential: Option<&str>,
    ) -> (AppState, Arc<CompanyRuntime>, Delivered) {
        use crate::ports::{CompanyStore, types::CompanyRecord};
        use crate::store::FsCompanyStore;

        let id = crate::ports::types::CompanyId::new("acme");
        let manifest: crate::company::CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n[policy]\nmode = \"full\"\n",
        )
        .expect("manifest");
        FsCompanyStore::new(home.to_path_buf())
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
                overlay_tool_grants: None,
                overlay_desk_tools: Default::default(),
                overlay_desks: Vec::new(),
                overlay_workflows: Vec::new(),
                overlay_budgets: Vec::new(),
                overlay_policy: None,
                disabled_workflows: Vec::new(),
                template_provenance: None,
                setup: None,
                name_confirmed: false,
                activation_completed_at: None,
                created_at_millis: None,
            })
            .await
            .expect("save company");

        let delivered: Delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let runtime = Arc::new(
            crate::runtime::RuntimeBuilder::new(home.to_path_buf(), manifest)
                .with_id(id.clone())
                .with_brain(Arc::new(RecordingBrain(delivered.clone())))
                .build()
                .await
                .expect("runtime"),
        );
        if let Some(credential) = credential {
            runtime
                .secrets()
                .set(
                    runtime.id(),
                    WEBHOOK_SECRET_KEY,
                    crate::ports::types::SecretValue(credential.to_string()),
                )
                .await
                .expect("store credential");
        }
        let state = AppState::new(crate::AppConfig::default());
        state.registry().insert(id, runtime.clone());
        (state, runtime, delivered)
    }

    /// Posts `body` to the route, with `auth` verbatim as the header value.
    async fn post_event(state: &AppState, auth: Option<&str>, body: Value) -> (StatusCode, Value) {
        use axum::body::{Body, to_bytes};
        use tower::ServiceExt;

        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri("/hooks/acme/chargebee")
            .header("content-type", "application/json");
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        let request = request.body(Body::from(body.to_string())).expect("request");
        let response = crate::server::router(state.clone())
            .oneshot(request)
            .await
            .expect("routed");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn paid_event() -> Value {
        json!({
            "event_type": "payment_succeeded",
            "content": {
                "invoice": {"id": "inv_1", "currency_code": "USD", "total": 10000},
                "customer": {"id": "cus_1", "email": "alan@tinyhumans.ai"}
            }
        })
    }

    #[tokio::test]
    async fn an_unverifiable_delivery_is_refused_and_never_becomes_an_event() {
        let home = tempfile::tempdir().expect("tempdir");
        // base64("cbuser:cbpass")
        let (state, _runtime, delivered) = state_with(home.path(), Some("cbuser:cbpass")).await;

        for (label, auth) in [
            ("no header at all", None),
            ("a wrong password", Some("Basic Y2J1c2VyOndyb25n")),
            ("a bearer token", Some("Bearer Y2J1c2VyOmNicGFzcw==")),
            ("a malformed encoding", Some("Basic QQ=garbage")),
        ] {
            let (status, body) = post_event(&state, auth, paid_event()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{label}: {body}");
            assert_eq!(body["code"], "unauthorized", "{label}");
        }

        // The `401` is the visible half. The half that matters is that none of
        // those four reached a cycle: an unverifiable POST must not be able to
        // drive the company at all.
        assert!(
            delivered.lock().expect("lock").is_empty(),
            "an unverifiable delivery drove a cycle: {:?}",
            delivered.lock().expect("lock"),
        );
    }

    #[tokio::test]
    async fn a_company_with_no_stored_credential_accepts_nothing() {
        // Fail closed: an unconfigured webhook must not be an open endpoint.
        // Without the stored-secret check, "no credential" would be the one
        // state in which any caller could drive a company cycle.
        let home = tempfile::tempdir().expect("tempdir");
        let (state, runtime, _delivered) = state_with(home.path(), None).await;

        let (status, _) =
            post_event(&state, Some("Basic Y2J1c2VyOmNicGFzcw=="), paid_event()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "nothing stored");

        // And an EMPTY stored value counts as unconfigured, which is how the
        // console clears a credential — the secret port has no delete.
        runtime
            .secrets()
            .set(
                runtime.id(),
                WEBHOOK_SECRET_KEY,
                crate::ports::types::SecretValue(String::new()),
            )
            .await
            .expect("clear");
        let (status, _) =
            post_event(&state, Some("Basic Y2J1c2VyOmNicGFzcw=="), paid_event()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "cleared to empty");
    }

    #[tokio::test]
    async fn a_verified_delivery_is_accepted_and_actually_raises_the_event() {
        // The `200` proves almost nothing on its own: this route answers `200`
        // for an ignored event, an unparseable body, and a paused company too. A
        // handler that dropped the `CompanyEvent::WebhookReceived` construction
        // or the `run_cycle` call entirely would still satisfy it — and the push
        // is the ONLY thing this route does that a live read cannot, so a
        // regression there silently removes the whole point of the endpoint.
        //
        // So the assertion is on what reached the brain.
        let home = tempfile::tempdir().expect("tempdir");
        let (state, _runtime, delivered) = state_with(home.path(), Some("cbuser:cbpass")).await;

        let (status, body) =
            post_event(&state, Some("Basic Y2J1c2VyOmNicGFzcw=="), paid_event()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(
            body["ignored"],
            Value::Null,
            "an acted-on event is not ignored"
        );

        let events = delivered.lock().expect("lock").clone();
        let raised = events
            .iter()
            .find_map(|event| match event {
                CompanyEvent::WebhookReceived { channel, body } if channel == CHANNEL => {
                    Some(body.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!("no WebhookReceived on the `{CHANNEL}` channel reached a cycle: {events:?}")
            });

        // And it carries the summary the brain is meant to relay, not the raw
        // Chargebee payload — the projection is part of the contract, since the
        // whole event would spend a great deal of context to say "Alan paid".
        assert_eq!(raised["event_type"], "payment_succeeded", "{raised}");
        let summary = raised["summary"].as_str().unwrap_or_default();
        assert!(summary.contains("inv_1"), "{summary}");
        assert!(summary.contains("PAID"), "{summary}");
        // The customer id, never their email — this body is persisted in the
        // journal and replayed into model prompts.
        assert!(summary.contains("cus_1"), "{summary}");
        assert!(!raised.to_string().contains("alan@"), "{raised}");
    }

    #[tokio::test]
    async fn a_verified_but_unsubscribed_event_never_reaches_a_cycle() {
        // The `ignored` field in the response says the route decided to skip it.
        // This says it actually did: an over-subscribed dashboard must not wake
        // the company on every subscription change it happens to send.
        let home = tempfile::tempdir().expect("tempdir");
        let (state, _runtime, delivered) = state_with(home.path(), Some("cbuser:cbpass")).await;

        post_event(
            &state,
            Some("Basic Y2J1c2VyOmNicGFzcw=="),
            json!({"event_type": "subscription_created", "content": {}}),
        )
        .await;
        assert!(
            delivered.lock().expect("lock").is_empty(),
            "an unsubscribed event drove a cycle: {:?}",
            delivered.lock().expect("lock"),
        );
    }

    #[tokio::test]
    async fn a_verified_but_unsubscribed_event_is_acknowledged_and_ignored() {
        // 2xx on purpose: answering non-2xx would make Chargebee retry, then
        // disable the endpoint, over an event we simply had no interest in.
        let home = tempfile::tempdir().expect("tempdir");
        let (state, _runtime, _delivered) = state_with(home.path(), Some("cbuser:cbpass")).await;

        let (status, body) = post_event(
            &state,
            Some("Basic Y2J1c2VyOmNicGFzcw=="),
            json!({"event_type": "subscription_created", "content": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ignored"], "subscription_created");
    }

    #[test]
    fn basic_auth_decodes_to_the_user_pass_pair() {
        // base64("cbuser:cbpass")
        assert_eq!(
            decode_basic("Basic Y2J1c2VyOmNicGFzcw==").as_deref(),
            Some("cbuser:cbpass")
        );
        // Anything that is not Basic is not ours to interpret.
        assert_eq!(decode_basic("Bearer abc"), None);
        assert_eq!(decode_basic("Basic !!!not base64!!!"), None);

        // Malformed input must be REFUSED, not decoded to a prefix that then
        // reaches a credential comparison.
        assert_eq!(decode_basic("Basic QQ"), None, "length not a multiple of 4");
        assert_eq!(decode_basic("Basic QQ=garbage"), None, "data after padding");
        assert_eq!(decode_basic("Basic ===="), None, "padding only");
        assert_eq!(decode_basic("Basic "), None, "empty");
        assert_eq!(decode_basic("Basic QUJD!"), None, "alphabet violation");
        // Canonical padding still works.
        assert_eq!(decode_basic("Basic QUJD").as_deref(), Some("ABC"));
        assert_eq!(decode_basic("Basic QUI=").as_deref(), Some("AB"));
    }

    #[test]
    fn a_long_credential_decodes_exactly() {
        // The accumulator is never masked, so it visibly "overflows" a u32 after
        // a handful of characters. That is fine and this pins why: `<<` in Rust
        // only panics when the SHIFT AMOUNT reaches the width — a constant 6
        // here — and bits pushed off the top are discarded by definition, while
        // the decoder only ever reads the low `nbits` (at most 6) it just wrote.
        // A wider type or a mask would change nothing.
        fn encode(input: &[u8]) -> String {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for chunk in input.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                out.push(A[(n >> 18 & 63) as usize] as char);
                out.push(A[(n >> 12 & 63) as usize] as char);
                out.push(if chunk.len() > 1 {
                    A[(n >> 6 & 63) as usize] as char
                } else {
                    '='
                });
                out.push(if chunk.len() > 2 {
                    A[(n & 63) as usize] as char
                } else {
                    '='
                });
            }
            out
        }

        for len in [1usize, 2, 3, 300, 30_000] {
            let credential = format!("cbuser:{}", "x".repeat(len));
            let decoded = decode_basic(&format!("Basic {}", encode(credential.as_bytes())));
            assert_eq!(
                decoded.as_deref(),
                Some(credential.as_str()),
                "a {len}-byte password must round-trip exactly"
            );
        }
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        // A length difference must not be reported as equal.
        assert!(!constant_time_eq(b"secret", b"secretx"));
    }

    #[test]
    fn only_the_three_billing_events_are_acted_on() {
        // Over-subscribing in the Chargebee dashboard is the normal case; an
        // unlisted event must be ignorable, not a reason to retry.
        assert!(ACTED_ON.contains(&"payment_succeeded"));
        assert!(ACTED_ON.contains(&"payment_failed"));
        assert!(!ACTED_ON.contains(&"subscription_created"));
        // The names #788 uses do not exist in Chargebee; if these ever start
        // matching, the issue's names were adopted and this test should say so.
        assert!(!ACTED_ON.contains(&"invoice_paid"));
    }

    #[test]
    fn a_paid_summary_names_the_invoice_amount_and_payer() {
        let event = json!({
            "event_type": "payment_succeeded",
            "content": {
                "invoice": {"id": "inv_42", "total": 10000, "currency_code": "USD"},
                "customer": {"id": "cus_7", "email": "alan@tinyhumans.ai"}
            }
        });
        let text = summarize("payment_succeeded", &event);
        assert!(text.contains("inv_42"), "{text}");
        assert!(text.contains("PAID"), "{text}");
        // The id identifies them; the EMAIL must not travel. This string is
        // persisted in the journal and replayed into model prompts, so a
        // counterparty's address would outlive the notification.
        assert!(text.contains("cus_7"), "{text}");
        assert!(!text.contains("alan@tinyhumans.ai"), "email leaked: {text}");
        // The unit must travel with the number or $100 gets reported as $10,000.
        assert!(text.contains("minor units"), "{text}");
    }

    #[test]
    fn a_failed_summary_says_the_invoice_is_still_outstanding() {
        let event = json!({
            "event_type": "payment_failed",
            "content": {"invoice": {"id": "inv_9", "total": 500, "currency_code": "USD"}}
        });
        let text = summarize("payment_failed", &event);
        assert!(text.contains("FAILED"), "{text}");
        assert!(text.contains("outstanding"), "{text}");
        // No customer object in the payload is normal; it must not panic or
        // render an empty gap where a person should be.
        assert!(text.contains("the customer"), "{text}");
    }

    #[test]
    fn a_summary_survives_a_payload_with_no_content() {
        let text = summarize(
            "payment_succeeded",
            &json!({"event_type": "payment_succeeded"}),
        );
        assert!(text.contains("(unknown)"), "{text}");
        assert!(text.contains("an unknown amount"), "{text}");
    }
}
