#![cfg(feature = "openhuman")]
//! **Proof that a company boots, works and finishes a card with no network.**
//!
//! Issue #579. Running OpenCompany with no cloud dependency was mostly true and
//! nowhere proven, and an unproven offline path rots the first time a cloud call
//! is added to a shared code path — silently, because nothing fails.
//!
//! # What this covers, and what it deliberately does not
//!
//! Covered: manifest-configured local inference (`provider = "ollama"` against an
//! OpenAI-compatible endpoint), the filesystem store, the embedded OpenHuman
//! agent runtime, the HTTP surface, loopback magic-link sign-in, and a card
//! driven from creation to `done`.
//!
//! **Not covered, by design**: the Medulla hosted brain, Composio, and the hub
//! identity exchange. Those are hosted services and are not local; #579 says so
//! outright. A green run of this file says the *offline* path works, never that
//! everything works offline. See `docs/spec/runtime/offline.md`.
//!
//! # Why the endpoint is a stub rather than a real Ollama
//!
//! `provider = "ollama"` resolves through the same code as any other
//! OpenAI-compatible endpoint — one `base_url`, no bearer when keyless
//! (`harness::provider`'s `request_plan_omits_bearer_for_keyless_ollama`), the
//! same request shape — and the URL is overridable. So pointing it at a scripted
//! loopback endpoint exercises the real `ollama` provider path while keeping the
//! lane fast and deterministic.
//!
//! What that does **not** prove is that a real Ollama server is wire-compatible
//! with this client; that needs a model pull, which is slow and fails for reasons
//! unrelated to this repo. Stated here rather than left for a reader to assume.
//!
//! # The egress guard
//!
//! The lane runs this under `sudo unshare -n` (see `.github/workflows/ci.yml`),
//! which gives the process a network namespace with nothing but loopback.
//!
//! `a_deliberate_outbound_call_fails` is what makes that a proof rather than a
//! claim: without it, a lane whose sandbox silently failed to apply would pass
//! exactly as a lane whose sandbox worked, and "nothing dialled out" would be
//! indistinguishable from "the namespace was not applied". It asserts only when
//! `OPENCOMPANY_OFFLINE_LANE=1` — the lane sets it — so the same file still runs
//! on a developer's networked laptop without failing for having a network.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use opencompany::company::CompanyManifest;
use opencompany::runtime::{RuntimeBuilder, company_id_from_name};
use opencompany::{AppConfig, AppState};

/// Set by the CI lane to say "you are inside the namespace; prove it".
const LANE_ENV: &str = "OPENCOMPANY_OFFLINE_LANE";

/// Whether this process is the sandboxed lane rather than a local run.
fn in_offline_lane() -> bool {
    std::env::var(LANE_ENV).is_ok_and(|v| v == "1")
}

// ---------------------------------------------------------------------------
// The scripted local endpoint
// ---------------------------------------------------------------------------

/// One scripted model turn.
#[derive(Clone, Debug)]
enum Turn {
    /// Finish the turn with plain assistant text.
    Say(&'static str),
}

/// A scripted OpenAI-compatible endpoint, served on loopback.
///
/// `/embeddings` is served alongside `/chat/completions` because the host's
/// embeddings client shares the same `base_url` and validates the width it gets
/// back — without it a memory write 404s in the middle of a turn, which reads as
/// an inference failure and is not one.
struct Script {
    turns: Mutex<Vec<Turn>>,
    seen: Mutex<Vec<Value>>,
}

async fn spawn_script(turns: Vec<Turn>) -> (String, Arc<Script>) {
    let script = Arc::new(Script {
        turns: Mutex::new(turns),
        seen: Mutex::new(Vec::new()),
    });
    let chat = Arc::clone(&script);
    let app = axum::Router::new()
        .route(
            "/chat/completions",
            post(move |Json(body): Json<Value>| {
                let script = Arc::clone(&chat);
                async move {
                    script.seen.lock().unwrap().push(body);
                    let next = {
                        let mut turns = script.turns.lock().unwrap();
                        (!turns.is_empty()).then(|| turns.remove(0))
                    };
                    // Running off the end means the loop turned more than
                    // scripted; end it with text rather than hanging.
                    let Turn::Say(text) = next.unwrap_or(Turn::Say("done"));
                    Json(json!({
                        "choices": [{
                            "index": 0,
                            "message": { "role": "assistant", "content": text },
                            "finish_reason": "stop"
                        }],
                        "usage": { "prompt_tokens": 12, "completion_tokens": 4 }
                    }))
                }
            }),
        )
        .route(
            "/embeddings",
            post(|Json(_body): Json<Value>| async move {
                Json(json!({
                    "data": [{ "index": 0, "embedding": vec![0.0_f32; 1536] }],
                    "usage": { "prompt_tokens": 1, "total_tokens": 1 }
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), script)
}

// ---------------------------------------------------------------------------
// The offline company
// ---------------------------------------------------------------------------

/// The manifest `docs/spec/runtime/offline.md` documents, with `base_url` bound
/// to the scripted endpoint instead of Ollama's default port.
///
/// Everything hosted is absent rather than configured-and-broken: no `composio`
/// grant, no managed credential, no Medulla. #579 asks for exactly that — a
/// surface that degrades by being missing, not by failing at call time.
fn offline_manifest(base_url: &str) -> String {
    format!(
        r#"
[company]
name = "Offline Co"
summary = "Proves the no-network path."

[inference]
provider = "ollama"
base_url = "{base_url}"
model = "llama3"

[tools]
allow = ["workspace", "files"]

[users]
admins = ["operator@opencompany.local"]

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"

[[agent]]
id = "writer"
role = "Writer"
"#
    )
}

/// Boots the offline company on loopback and returns its address and id.
async fn boot(home: &std::path::Path, base_url: &str) -> (SocketAddr, String) {
    let mut manifest = CompanyManifest::from_stored_toml(&offline_manifest(base_url))
        .expect("the documented offline manifest parses");
    manifest.apply_globals();
    let problems = manifest.validate();
    assert!(
        problems.is_empty(),
        "the documented offline manifest must be valid: {problems:?}"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let company_id = company_id_from_name(&manifest.company.name);
    let state = AppState::new(AppConfig {
        bind: address.to_string(),
        ..AppConfig::default()
    })
    .with_home(home.to_path_buf());
    let runtime = RuntimeBuilder::new(state.home().to_path_buf(), manifest)
        .with_id(company_id.clone())
        // The agent pool. `serve` wires this (`src/bin/opencompany.rs`); without
        // it `dispatch_task` is a documented no-op and the board stays inert, so
        // a card would sit in In Progress forever and the lane would prove
        // nothing about an agent working offline.
        .with_harness(Arc::new(opencompany::harness::HarnessPool::new()))
        .build()
        .await
        .expect("the offline company builds with no network");
    state
        .registry()
        .insert(company_id.clone(), Arc::new(runtime));
    tokio::spawn(async move {
        let _ = opencompany::server::serve_on(listener, state).await;
    });
    (address, company_id.as_ref().to_string())
}

#[tokio::test]
async fn a_deliberate_outbound_call_fails_inside_the_namespace() {
    if !in_offline_lane() {
        eprintln!(
            "[offline] {LANE_ENV} is unset, so this is a local run with a network; \
             skipping the egress assertion. The CI lane sets it inside `unshare -n`."
        );
        return;
    }
    // A routable public address, dialled directly so no DNS is needed — the
    // failure must be "there is no route", not "the name did not resolve",
    // which a namespace with no interfaces gives us either way.
    let attempt = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect("1.1.1.1:443"),
    )
    .await;
    // A refusal, an unreachable network, or a timeout all mean the same thing
    // here: nothing got out. Only a *connection* is a failure.
    if let Ok(Ok(_)) = attempt {
        panic!(
            "egress SUCCEEDED inside the offline lane, so the network namespace was not applied \
             and every other assertion in this file proves nothing about being offline"
        );
    }
}

// ---------------------------------------------------------------------------
// The end-to-end path
// ---------------------------------------------------------------------------

/// A tiny cookie-carrying HTTP helper. `reqwest`'s cookie store is behind a
/// feature this crate does not enable, so the session cookie is carried by hand.
struct Client {
    inner: reqwest::Client,
    base: String,
    cookie: Mutex<Option<String>>,
}

impl Client {
    fn new(address: SocketAddr) -> Self {
        Self {
            inner: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            base: format!("http://{address}"),
            cookie: Mutex::new(None),
        }
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self.inner.request(method, format!("{}{path}", self.base));
        if let Some(cookie) = self.cookie.lock().unwrap().clone() {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("the loopback host answers");
        let status = response.status().as_u16();
        if let Some(set) = response.headers().get(reqwest::header::SET_COOKIE)
            && let Ok(value) = set.to_str()
            && let Some((pair, _)) = value.split_once(';')
        {
            *self.cookie.lock().unwrap() = Some(pair.to_string());
        }
        let text = response.text().await.unwrap_or_default();
        let json = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, json)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }
    async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }

    /// Signs in as the manifest admin over the loopback magic-link flow, which
    /// echoes the code rather than mailing it — there is no mail transport here,
    /// and on a routable host it would not echo at all.
    async fn sign_in(&self, email: &str) {
        let (status, body) = self
            .post("/api/v1/company/auth/request", json!({ "email": email }))
            .await;
        assert_eq!(status, 200, "sign-in request refused: {body}");
        let code = body["dev_code"]
            .as_str()
            .unwrap_or_else(|| panic!("no dev_code came back, so no session can be minted: {body}"))
            .to_string();
        let (status, body) = self
            .post("/api/v1/company/auth/verify", json!({ "code": code }))
            .await;
        assert_eq!(status, 200, "the login code was refused: {body}");
    }
}

/// Polls `GET /tasks/{id}` until its column is one of `wanted`, or gives up.
async fn wait_for_column(client: &Client, task_id: &str, wanted: &[&str]) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        let (status, body) = client
            .get(&format!("/api/v1/company/tasks/{task_id}"))
            .await;
        if status == 200 {
            let column = body
                .get("task")
                .and_then(|task| task.get("column"))
                .or_else(|| body.get("column"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if wanted.contains(&column.as_str()) {
                return column;
            }
            last = column;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("card never reached {wanted:?}; it is sitting in `{last}`");
}

/// **The acceptance path.** A company boots, an agent works a card, and the card
/// reaches `done` — with no network.
///
/// ## Why the operator's move is part of the path rather than a shortcut
///
/// #579 asks for a card driven "to Done". Since the operator decision of
/// 2026-08-05 (recorded in `harness::lifecycle`), **`done` is reachable only by a
/// person**: every card — delegated or board-created — settles in `in_review`
/// first, and an approving verdict is what moves it. So the agent half of this
/// test drives the card as far as an agent can take it, offline and unaided, and
/// the harness then plays the operator who approves it.
///
/// That is not a weakening of the claim; it is the claim stated accurately. A
/// version of this test that reported `in_review` as "Done" would be reading the
/// lane as proof that an agent finishes work alone, which the product does not
/// permit.
#[tokio::test]
async fn a_company_boots_works_a_card_and_reaches_done_with_no_network() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) =
        spawn_script(vec![Turn::Say("The brief is written and ready to review.")]).await;
    let (address, _company) = boot(home.path(), &base_url).await;
    let client = Client::new(address);

    let (status, _) = client.get("/healthz").await;
    assert_eq!(status, 200, "the offline host serves /healthz");

    client.sign_in("operator@opencompany.local").await;

    let (status, card) = client
        .post(
            "/api/v1/company/tasks",
            json!({ "title": "Write the launch brief", "assignee": "writer" }),
        )
        .await;
    assert_eq!(status, 200, "card creation refused: {card}");
    let task_id = card["id"]
        .as_str()
        .expect("the new card has an id")
        .to_string();

    // The board drag that starts work — `upsert_task` reads this edge and
    // dispatches, which is what puts the agent (and therefore the local
    // endpoint) on the path.
    let (status, body) = client
        .patch(
            &format!("/api/v1/company/tasks/{task_id}"),
            json!({ "column": "in_progress" }),
        )
        .await;
    assert_eq!(status, 200, "dispatch refused: {body}");

    let settled = wait_for_column(&client, &task_id, &["in_review", "done", "paused"]).await;
    assert_eq!(
        settled, "in_review",
        "an agent takes a card as far as In Review and no further"
    );

    // The person. `done` has exactly one route and it runs through a human.
    let (status, body) = client
        .patch(
            &format!("/api/v1/company/tasks/{task_id}"),
            json!({ "column": "done" }),
        )
        .await;
    assert_eq!(
        status, 200,
        "the operator's approving move was refused: {body}"
    );
    let finished = wait_for_column(&client, &task_id, &["done"]).await;
    assert_eq!(finished, "done");

    // The load-bearing assertion about *offline*: the work above actually went
    // through the local endpoint. Without this the test would pass just as well
    // against a card that never reached a model at all.
    let calls = script.seen.lock().unwrap().len();
    assert!(
        calls > 0,
        "no inference request reached the local endpoint, so this proves nothing about \
         a locally-served model driving the work"
    );
}
