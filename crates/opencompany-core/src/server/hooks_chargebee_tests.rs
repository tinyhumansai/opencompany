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
            general_channel: Default::default(),
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

    let (status, _) = post_event(&state, Some("Basic Y2J1c2VyOmNicGFzcw=="), paid_event()).await;
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
    let (status, _) = post_event(&state, Some("Basic Y2J1c2VyOmNicGFzcw=="), paid_event()).await;
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

    let (status, body) = post_event(&state, Some("Basic Y2J1c2VyOmNicGFzcw=="), paid_event()).await;
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
