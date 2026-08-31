//! One operator message opens **one** card — issue #463.
//!
//! Three independent paths can card the same message: the REST chat handler's
//! `detect_task_intent`, the delegation seam's `open_work_card` (#442), and the
//! publish drain's minted card (#445). Each was correct alone. Two of them in
//! one turn produced two cards, and the reply bubble linked to the one with no
//! artifacts on it, so the operator clicked through to an empty card while the
//! deliverable sat on an orphan.
//!
//! No unit test could see it: the doubling only exists where all three paths are
//! wired together. So this stands up a real host — real `FsOps` stores, real
//! platform auth, the production `server::router` on a real TCP socket — and
//! drives it over HTTP, with a scripted OpenAI-compatible endpoint so a real
//! model turn can emit `delegate_to_desk` and `publish_artifact`.
//!
//! The script is keyed on **which agent is asking and what it has already
//! seen**, not on call order: the orchestrator turn, the desk turn and the relay
//! turn interleave, and an order-keyed script answers the wrong one.
//!
//! Feature-gated on `openhuman`, which is what wires the harness pool, the
//! hosted inference provider and `reqwest`.
#![cfg(feature = "openhuman")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

use opencompany::app::{AppConfig, AppState};
use opencompany::company::CompanyManifest;
use opencompany::company::credentials::Credential;
use opencompany::company::task_intent::detect_task_intent;
use opencompany::harness::HarnessPool;
use opencompany::harness::provider::HostedProviderConfig;
use opencompany::ports::CompanyId;
use opencompany::runtime::RuntimeBuilder;
use opencompany::server::platform_auth::{PlatformAuthConfig, StaticPlatformVerifier};

const TOKEN: &str = "test-platform-token";
const COMPANY: &str = "acme";

// ---------------------------------------------------------------------------
// The scripted model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Turn {
    /// Emit one tool call.
    Call { tool: String, args: Value },
    /// Emit several tool calls in ONE assistant message (for the cap case).
    Calls(Vec<(String, Value)>),
    /// Finish with plain assistant text.
    Say(String),
}

fn call(tool: &str, args: Value) -> Turn {
    Turn::Call {
        tool: tool.into(),
        args,
    }
}

fn say(text: &str) -> Turn {
    Turn::Say(text.into())
}

/// A scripted reaction: fires the first time its predicate matches, then is
/// consumed.
struct Rule {
    when: Box<dyn Fn(&Ctx) -> bool + Send>,
    then: Turn,
    /// Run against the live company *before* answering.
    effect: Effect,
}

/// Something a rule does to the running company on its way past.
///
/// The scripted endpoint is called from inside the cycle, which makes it the
/// only place a test can mutate the board **while the turn is still in
/// flight** — and that is the only way to reach the vanished-card path from
/// outside the process.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Effect {
    None,
    /// Delete every card on the board.
    ClearBoard,
}

/// What the endpoint can see about one request.
struct Ctx {
    /// The `role` line of the system prompt, e.g. "Chief Executive".
    role: String,
    /// Text of every `tool` message already in the transcript.
    tool_results: Vec<String>,
    /// The last user message.
    user: String,
}

/// The persona's opening line, which is what identifies the agent asking.
///
/// Not simply the prompt's first line. OpenHuman prepends a `## Tool Policy
/// Boundary` block to the whole assembled prompt when the session has tool
/// restrictions (`agent/harness/session/turn/context.rs`), so the persona is no
/// longer at the top. Keying on line one silently matched that heading for
/// every agent, no rule fired, and each turn fell through to the endpoint's
/// plain-text default — which reads here as "the turn opened no card" rather
/// than as "the script stopped matching".
fn persona_line(system_prompt: &str) -> String {
    let mut lines = system_prompt.lines();
    if system_prompt.starts_with("## Tool Policy Boundary") {
        // The block is a heading, its bullet list, then a blank line.
        for line in lines.by_ref() {
            if line.trim().is_empty() {
                break;
            }
        }
    }
    lines
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

/// The persona opening line is found past the tool-policy block for both the
/// unnamed form ("You are the {role} …") and the named form ("You are {name},
/// the {role} …"). An agent whose name differs from its role opens with the
/// named form; if that line were read as empty, no rule would fire for it.
#[test]
fn persona_line_skips_the_tool_policy_boundary_for_both_persona_forms() {
    let base = "\
## Tool Policy Boundary
- Agent: ceo
- Channel: web
- Entry point: chat
- Allowed permission: supervised
- Risk: low

";
    let unnamed = format!(
        "{base}You are the Chief Executive at Acme. Speak in the first person as this role."
    );
    assert_eq!(
        persona_line(&unnamed),
        "You are the Chief Executive at Acme. Speak in the first person as this role."
    );
    let named = format!(
        "{base}You are Alex, the Content Writer at Acme. Speak in the first person as this role."
    );
    assert_eq!(
        persona_line(&named),
        "You are Alex, the Content Writer at Acme. Speak in the first person as this role."
    );
}

impl Ctx {
    fn of(body: &Value) -> Self {
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        let text = |m: &Value| m["content"].as_str().unwrap_or("").to_string();
        let role = persona_line(
            &msgs
                .iter()
                .find(|m| m["role"] == "system")
                .map(text)
                .unwrap_or_default(),
        );
        Ctx {
            role,
            tool_results: msgs
                .iter()
                .filter(|m| m["role"] == "tool")
                .map(text)
                .collect(),
            user: msgs
                .iter()
                .filter(|m| m["role"] == "user")
                .map(text)
                .next_back()
                .unwrap_or_default(),
        }
    }

    /// The agent answering this request.
    fn is(&self, role: &str) -> bool {
        self.role.contains(role)
    }

    /// Its first turn — nothing has come back from a tool yet.
    fn fresh(&self) -> bool {
        self.tool_results.is_empty()
    }

    /// The CEO-relay hand-back — the extra orchestrator turn that runs *after*
    /// the desk answered and its card settled.
    ///
    /// Matched on the relay prompt's own wording rather than on turn ordering,
    /// because the orchestrator's first turn continues after `delegate_to_desk`
    /// returns and that continuation is also a non-fresh orchestrator request.
    /// A rule keyed only on "the orchestrator, not fresh" lands on the
    /// continuation, which runs *before* the hand-off has opened its card.
    fn is_relay(&self) -> bool {
        self.user
            .contains("Relay their answer back to the operator")
    }
}

fn rule(when: impl Fn(&Ctx) -> bool + Send + 'static, then: Turn) -> Rule {
    Rule {
        when: Box::new(when),
        then,
        effect: Effect::None,
    }
}

/// A rule that wipes the board on its way past — see [`Effect`].
fn rule_clearing_the_board(when: impl Fn(&Ctx) -> bool + Send + 'static, then: Turn) -> Rule {
    Rule {
        effect: Effect::ClearBoard,
        ..rule(when, then)
    }
}

struct Script {
    rules: Mutex<Vec<Rule>>,
    /// Every request body the endpoint saw, so a test can assert on what an
    /// agent was actually shown rather than on what it presumably was.
    seen: Mutex<Vec<Value>>,
    /// The company's own base URL, registered once the host is up so an
    /// [`Effect`] can reach back into it mid-turn.
    host: Mutex<Option<String>>,
    /// How many cards each fired [`Effect::ClearBoard`] actually deleted.
    ///
    /// Recorded rather than asserted in place, because **a panic inside this
    /// endpoint is invisible to the test**: the agent turn treats a failed
    /// inference call as a recoverable turn, so the cycle completes and the
    /// case goes green over a scenario that never happened. Found exactly that
    /// way — the first draft of the deleted-card test passed while its rule was
    /// panicking on an empty board. The test asserts on this instead.
    cleared: Mutex<Vec<usize>>,
}

impl Script {
    /// The last user message each turn by `role` was given.
    fn prompts_to(&self, role: &str) -> Vec<String> {
        self.seen
            .lock()
            .expect("seen")
            .iter()
            .map(Ctx::of)
            .filter(|ctx| ctx.is(role))
            .map(|ctx| ctx.user)
            .collect()
    }

    /// Deletes every card, from inside the turn that is running, and records how
    /// many went — see [`Script::cleared`] for why it records rather than
    /// asserts.
    async fn clear_board(&self) {
        let Some(base) = self.host.lock().expect("host").clone() else {
            return;
        };
        let client = reqwest::Client::new();
        let Ok(res) = client
            .get(format!("{base}/api/v1/companies/{COMPANY}/tasks"))
            .bearer_auth(TOKEN)
            .send()
            .await
        else {
            return;
        };
        let cards: Vec<Value> = res.json().await.unwrap_or_default();
        let mut deleted = 0;
        for card in cards {
            let Some(id) = card["id"].as_str() else {
                continue;
            };
            let deleted_ok = client
                .delete(format!("{base}/api/v1/companies/{COMPANY}/tasks/{id}"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            deleted += usize::from(deleted_ok);
        }
        self.cleared.lock().expect("cleared").push(deleted);
    }

    /// How many cards each fired board-clearing rule deleted, in order.
    fn cleared(&self) -> Vec<usize> {
        self.cleared.lock().expect("cleared").clone()
    }
}

fn tool_call_message(calls: &[(String, Value)]) -> Value {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (tool, args))| {
            json!({
                "id": format!("call_{i}"),
                "type": "function",
                "function": { "name": tool, "arguments": args.to_string() }
            })
        })
        .collect();
    json!({ "role": "assistant", "content": null, "tool_calls": tool_calls })
}

/// Serves the scripted endpoint on a loopback port. An unmatched request is
/// answered with plain text, which ends that agent's turn.
async fn spawn_model(rules: Vec<Rule>) -> (String, Arc<Script>) {
    let script = Arc::new(Script {
        rules: Mutex::new(rules),
        seen: Mutex::new(Vec::new()),
        host: Mutex::new(None),
        cleared: Mutex::new(Vec::new()),
    });
    let handle = Arc::clone(&script);
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<Value>| {
            let script = Arc::clone(&handle);
            async move {
                script.seen.lock().expect("seen").push(body.clone());
                let ctx = Ctx::of(&body);
                let next = {
                    let mut rules = script.rules.lock().expect("rules");
                    rules
                        .iter()
                        .position(|r| (r.when)(&ctx))
                        .map(|i| rules.remove(i))
                        .map(|r| (r.then, r.effect))
                };
                let (next, effect) = match next {
                    Some((then, effect)) => (Some(then), effect),
                    None => (None, Effect::None),
                };
                if effect == Effect::ClearBoard {
                    script.clear_board().await;
                }
                let message = match next.unwrap_or_else(|| say("done")) {
                    Turn::Say(text) => json!({ "role": "assistant", "content": text }),
                    Turn::Call { tool, args } => tool_call_message(&[(tool, args)]),
                    Turn::Calls(calls) => tool_call_message(&calls),
                };
                Json(json!({
                    "id": "chatcmpl-test",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "chat-v1",
                    "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind model");
    let addr: SocketAddr = listener.local_addr().expect("model addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), script)
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

const MANIFEST: &str = r#"
[company]
name = "Acme"
output = "Written deliverables"
human_role = "Steering"

[policy]
mode = "full"

[tools]
allow = ["*"]

[[agent]]
id = "ceo"
role = "Chief Executive"
description = "Sets direction and delegates the work."
tier = "orchestrator"
tools = ["*"]

[[agent]]
id = "writer"
role = "Writer"
description = "Turns rough notes into short, clear written drafts."
tools = ["*"]

[[agent]]
id = "analyst"
role = "Analyst"
description = "Digs through numbers."
tools = ["*"]

[[agent]]
id = "engineer"
role = "Engineer"
description = "Builds things."
tools = ["*"]

[[group_chat]]
id = "content"
name = "Content desk"
description = "Written drafts and copy."
members = ["writer"]

[[group_chat]]
id = "research"
name = "Research desk"
description = "Numbers and analysis."
members = ["analyst"]

[[group_chat]]
id = "engineering"
name = "Engineering desk"
description = "How things are built."
members = ["engineer"]
"#;

struct Host {
    base: String,
    home: PathBuf,
    _tmp: tempfile::TempDir,
}

/// Stands up a company on a real router over a real socket, pointed at `model`.
async fn spawn_host(model: String) -> Host {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().to_path_buf();

    let company_dir = home.join("manifest");
    std::fs::create_dir_all(&company_dir).expect("company dir");
    std::fs::write(company_dir.join("company.toml"), MANIFEST).expect("manifest");
    let manifest = CompanyManifest::from_path(&company_dir).expect("valid manifest");

    let runtime = RuntimeBuilder::new(home.clone(), manifest)
        .with_id(CompanyId::new(COMPANY))
        .with_harness(Arc::new(HarnessPool::new()))
        .with_harness_inference(
            HostedProviderConfig {
                base_url: model,
                credential: Credential::from_value("scripted"),
                extra_headers: Vec::new(),
            },
            None,
        )
        .build()
        .await
        .expect("runtime builds");

    let state = AppState::new(AppConfig {
        bind: "127.0.0.1:0".parse().expect("bind"),
        ..AppConfig::default()
    })
    .with_home(home.clone())
    .with_platform_auth(PlatformAuthConfig::new(Arc::new(
        StaticPlatformVerifier::new(TOKEN),
    )));
    state
        .registry()
        .insert(CompanyId::new(COMPANY), Arc::new(runtime));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind host");
    let addr: SocketAddr = listener.local_addr().expect("host addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, opencompany::server::router(state)).await;
    });

    Host {
        base: format!("http://{addr}"),
        home,
        _tmp: tmp,
    }
}

impl Host {
    /// Mirrors `harness::build::agent_workspace(home/harness, company, agent)`.
    fn agent_sandbox(&self, agent: &str) -> PathBuf {
        self.home
            .join("harness")
            .join(COMPANY)
            .join(agent)
            .join("workspace")
    }

    /// Puts a file in an agent's sandbox so it has something real to publish.
    fn seed_file(&self, agent: &str, name: &str, body: &str) {
        let dir = self.agent_sandbox(agent);
        std::fs::create_dir_all(&dir).expect("sandbox");
        std::fs::write(dir.join(name), body).expect("seed file");
    }

    async fn chat(&self, text: &str, desk: Option<&str>) -> Value {
        let mut body = json!({ "text": text });
        if let Some(desk) = desk {
            body["chat"] = json!(desk);
        }
        let res = reqwest::Client::new()
            .post(format!("{}/api/v1/companies/{COMPANY}/chat", self.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .expect("chat request");
        let status = res.status();
        let text = res.text().await.expect("chat body");
        assert!(status.is_success(), "chat failed {status}: {text}");
        serde_json::from_str(&text).expect("chat json")
    }

    /// The board plus the card the reply linked to — the two facts #463 is about.
    async fn board(&self, reply: &Value) -> Board {
        let cards: Vec<Value> = reqwest::Client::new()
            .get(format!("{}/api/v1/companies/{COMPANY}/tasks", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("tasks request")
            .json()
            .await
            .expect("tasks json");
        let linked = reply["responses"]
            .as_array()
            .and_then(|r| r.iter().find_map(|m| m["taskId"].as_str()))
            .map(str::to_string);
        Board { cards, linked }
    }

    async fn artifacts(&self, task_id: &str) -> Vec<Value> {
        reqwest::Client::new()
            .get(format!(
                "{}/api/v1/companies/{COMPANY}/tasks/{task_id}/artifacts",
                self.base
            ))
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("artifacts request")
            .json()
            .await
            .expect("artifacts json")
    }
}

struct Board {
    cards: Vec<Value>,
    /// The `taskId` the operator's reply bubble points at, if any.
    linked: Option<String>,
}

impl Board {
    /// The single card this message opened. Panics — with the whole board in the
    /// message — when there is not exactly one, which is the assertion every
    /// case below is really making.
    fn only(&self) -> &Value {
        assert_eq!(
            self.cards.len(),
            1,
            "expected exactly one card for one message, got {}:{}",
            self.cards.len(),
            self.describe()
        );
        &self.cards[0]
    }

    fn describe(&self) -> String {
        self.cards
            .iter()
            .map(|c| {
                format!(
                    "\n  - id={} column={} assignee={:?} title={:?}",
                    c["id"].as_str().unwrap_or("?"),
                    c["column"].as_str().unwrap_or("?"),
                    c["assignee"].as_str().unwrap_or("?"),
                    c["title"].as_str().unwrap_or("?"),
                )
            })
            .collect()
    }
}

fn id_of(card: &Value) -> &str {
    card["id"].as_str().expect("card id")
}

/// `(published path, author of its first version)` for every artifact on a
/// card, sorted — the per-item attribution the artifact record actually stores.
async fn authors_on(host: &Host, task_id: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = host
        .artifacts(task_id)
        .await
        .iter()
        .map(|a| {
            (
                a["source"].as_str().unwrap_or("?").to_string(),
                a["versions"][0]["authorId"]
                    .as_str()
                    .unwrap_or("?")
                    .to_string(),
            )
        })
        .collect();
    pairs.sort();
    pairs
}

/// Asserts the invariant the issue is about: one card, it holds the delivered
/// file, the reply links to *it*, and it is filed under the agent that actually
/// published — not the responder that relayed for them.
async fn assert_sole_card_carries(host: &Host, board: &Board, publisher: &str, artifact: &str) {
    let card = board.only();
    let arts = host.artifacts(id_of(card)).await;
    let titles: Vec<&str> = arts
        .iter()
        .map(|a| a["title"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        titles,
        [artifact],
        "the one card must hold the deliverable, not an orphan beside it"
    );
    assert_eq!(
        board.linked.as_deref(),
        Some(id_of(card)),
        "the reply must link to the card holding the artifact"
    );
    assert_eq!(
        card["assignee"].as_str(),
        Some(publisher),
        "the card belongs to the agent that published, not the turn's responder"
    );
}

/// The rules for "orchestrator hands off; the desk drafts and publishes".
fn delegate_then_publish(
    desk: &'static str,
    worker: &'static str,
    title: &'static str,
) -> Vec<Rule> {
    vec![
        rule(
            move |c| c.is("Chief Executive") && c.fresh(),
            call(
                "delegate_to_desk",
                json!({ "desk": desk, "instruction": format!("Draft the {title} and publish it.") }),
            ),
        ),
        rule(
            move |c| c.is(worker) && c.fresh(),
            call(
                "publish_artifact",
                json!({ "path": "memo.md", "title": title }),
            ),
        ),
        rule(move |c| c.is(worker), say("Drafted and published it.")),
        rule(|c| c.is("Chief Executive"), say("The desk published it.")),
    ]
}

// ---------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------

/// The headline. A substantial ask that ends in a published file used to open
/// two cards — #442's, which won the reply link and held nothing, and #445's,
/// which held the deliverable and was reachable from nowhere.
///
/// The ask is deliberately *not* a leading imperative, so `detect_task_intent`
/// stays out of it and what is counted is #442's card against #445's.
#[tokio::test(flavor = "multi_thread")]
async fn a_substantial_ask_that_publishes_opens_one_card() {
    assert!(
        detect_task_intent("assemble the Q3 board pack").is_none(),
        "this case must NOT reach the REST detector, or it proves something else"
    );
    let (model, _script) =
        spawn_model(delegate_then_publish("content", "Writer", "Q3 board memo")).await;
    let host = spawn_host(model).await;
    host.seed_file("writer", "memo.md", "# Q3 board memo\n");

    let reply = host.chat("assemble the Q3 board pack", None).await;
    let board = host.board(&reply).await;
    assert_sole_card_carries(&host, &board, "writer", "Q3 board memo").await;
}

/// The same shape asked straight of a desk, where #442's *direct* path is what
/// opens the card rather than the hand-off path.
#[tokio::test(flavor = "multi_thread")]
async fn a_substantial_ask_to_a_desk_that_publishes_opens_one_card() {
    let (model, _script) = spawn_model(vec![
        rule(
            |c| c.is("Writer") && c.fresh(),
            call(
                "publish_artifact",
                json!({ "path": "memo.md", "title": "Q3 board memo" }),
            ),
        ),
        rule(|c| c.is("Writer"), say("Drafted and published it.")),
    ])
    .await;
    let host = spawn_host(model).await;
    host.seed_file("writer", "memo.md", "# Q3 board memo\n");

    let reply = host
        .chat("assemble the Q3 board pack", Some("content"))
        .await;
    let board = host.board(&reply).await;
    assert_sole_card_carries(&host, &board, "writer", "Q3 board memo").await;
}

/// The card the turn opened is **deleted mid-turn**, so the publish falls back
/// to minting a replacement. The reply must link to the card that holds the
/// deliverable, not to the id of the card that is gone.
///
/// This is #463's own failure reachable through the fallback added to prevent
/// it: `spawned_task` still names the deleted card, `published_card` names the
/// replacement, and preferring the former sends the operator to a card that no
/// longer exists — with the deliverable somewhere else.
///
/// The delete is fired from the **relay** turn, which is the one window that
/// works: the hand-off's card is settled by then (an earlier delete would be
/// undone by that settle) and the publish queue has not yet been drained.
#[tokio::test(flavor = "multi_thread")]
async fn a_publish_onto_a_card_deleted_mid_turn_links_the_reply_to_the_replacement() {
    let (model, script) = spawn_model(vec![
        rule(
            |c| c.is("Chief Executive") && c.fresh(),
            call(
                "delegate_to_desk",
                json!({ "desk": "content", "instruction": "Draft the Q3 board memo and publish it." }),
            ),
        ),
        rule(
            |c| c.is("Writer") && c.fresh(),
            call(
                "publish_artifact",
                json!({ "path": "memo.md", "title": "Q3 board memo" }),
            ),
        ),
        rule(|c| c.is("Writer"), say("Drafted and published it.")),
        rule_clearing_the_board(
            |c| c.is("Chief Executive") && c.is_relay(),
            say("The desk published it — and someone just cleared the board."),
        ),
    ])
    .await;
    let host = spawn_host(model).await;
    *script.host.lock().expect("host") = Some(host.base.clone());
    host.seed_file("writer", "memo.md", "# Q3 board memo\n");

    let reply = host.chat("assemble the Q3 board pack", None).await;

    // Prove the scenario happened before asserting anything about it: exactly
    // one clearing rule fired and it really deleted the hand-off's card.
    assert_eq!(
        script.cleared(),
        vec![1],
        "the card was never deleted, so this case proves nothing"
    );
    let board = host.board(&reply).await;
    // The deliverable is never dropped: a replacement card carries it, and the
    // reply points at THAT card rather than at the id that no longer resolves.
    assert_sole_card_carries(&host, &board, "writer", "Q3 board memo").await;
}

/// A recognised imperative that the orchestrator hands off. The REST handler
/// carded it before the cycle started; #442's stand-down lived only on the
/// direct path, so the hand-off opened a second one.
///
/// The fixture verb matters more than it looks: `do the quarterly close` never
/// reaches the REST detector — `do` is not one of its action verbs — so a test
/// written against it would pass while proving nothing. The assertion below is
/// the guard against writing that test by accident.
#[tokio::test(flavor = "multi_thread")]
async fn a_recognised_imperative_that_is_delegated_opens_one_card() {
    let imperative = "draft the quarterly close memo";
    assert!(
        detect_task_intent(imperative).is_some(),
        "the fixture must be a message the REST handler cards, or this proves nothing"
    );
    assert!(
        detect_task_intent("do the quarterly close").is_none(),
        "`do` is not an action verb — a test written against it would prove nothing"
    );
    let (model, _script) = spawn_model(vec![
        rule(
            |c| c.is("Chief Executive") && c.fresh(),
            call(
                "delegate_to_desk",
                json!({ "desk": "research", "instruction": "Draft the quarterly close memo." }),
            ),
        ),
        rule(|c| c.is("Analyst"), say("Drafted.")),
        rule(|c| c.is("Chief Executive"), say("Research drafted it.")),
    ])
    .await;
    let host = spawn_host(model).await;

    let reply = host.chat(imperative, None).await;
    let board = host.board(&reply).await;
    let card = board.only();
    // The handler's card is the card — and the reply now points at it, which it
    // did not before: the operator got a card on the board and a bubble that
    // mentioned nothing.
    assert_eq!(board.linked.as_deref(), Some(id_of(card)));
}

/// All three carding paths in one turn: a recognised imperative, handed off, and
/// published by the delegate. This is the case that used to produce three cards.
#[tokio::test(flavor = "multi_thread")]
async fn a_recognised_imperative_delegated_and_published_opens_one_card() {
    let imperative = "draft the quarterly close memo";
    assert!(detect_task_intent(imperative).is_some(), "fixture check");
    let (model, _script) = spawn_model(delegate_then_publish(
        "content",
        "Writer",
        "Quarterly close memo",
    ))
    .await;
    let host = spawn_host(model).await;
    host.seed_file("writer", "memo.md", "# Quarterly close\n");

    let reply = host.chat(imperative, None).await;
    let board = host.board(&reply).await;
    // The handler's To-do card had no owner; the publish files onto it and it
    // becomes the writer's, because they are who delivered.
    assert_sole_card_carries(&host, &board, "writer", "Quarterly close memo").await;
}

/// Two different agents publish in one turn, and each artifact records **its
/// own** author.
///
/// The desk lead's turn and the relay turn both run with the full toolbelt
/// under the same `Conversation` claim, so one drain can hold publishes from
/// more than one agent. `PendingPublish.agent` carries the truth per item; a
/// single publisher picked for the whole batch stamps the writer's name on the
/// orchestrator's file and the reverse.
///
/// The **card** still takes one owner — a card has one — so only the artifact
/// authors are per item.
#[tokio::test(flavor = "multi_thread")]
async fn two_agents_publishing_in_one_turn_each_keep_their_own_authorship() {
    let (model, _script) = spawn_model(vec![
        rule(
            |c| c.is("Chief Executive") && c.fresh(),
            call(
                "delegate_to_desk",
                json!({ "desk": "content", "instruction": "Draft the Q3 board memo and publish it." }),
            ),
        ),
        rule(
            |c| c.is("Writer") && c.fresh(),
            call(
                "publish_artifact",
                json!({ "path": "memo.md", "title": "Q3 board memo" }),
            ),
        ),
        rule(|c| c.is("Writer"), say("Drafted and published it.")),
        // The relay turn publishes too — the orchestrator's own covering note.
        rule(
            |c| c.is("Chief Executive"),
            call(
                "publish_artifact",
                json!({ "path": "notes.md", "title": "Covering note" }),
            ),
        ),
        rule(|c| c.is("Chief Executive"), say("Memo and covering note attached.")),
    ])
    .await;
    let host = spawn_host(model).await;
    host.seed_file("writer", "memo.md", "# Q3 board memo\n");
    host.seed_file("ceo", "notes.md", "# Covering note\n");

    let reply = host.chat("assemble the Q3 board pack", None).await;
    let board = host.board(&reply).await;
    let card = board.only();

    // `authors_on` sorts by path, so `memo.md` precedes `notes.md`.
    assert_eq!(
        authors_on(&host, id_of(card)).await,
        vec![
            ("memo.md".to_string(), "writer".to_string()),
            ("notes.md".to_string(), "ceo".to_string()),
        ],
        "each artifact must record the agent that published IT"
    );
    // The card keeps a single owner — the first publisher, who is also whose
    // card it already was.
    assert_eq!(card["assignee"].as_str(), Some("writer"));
}

/// A substantial ask with no publish still opens exactly one — the case #442
/// already got right, kept here so a fix to the publish path cannot regress it.
#[tokio::test(flavor = "multi_thread")]
async fn a_substantial_ask_without_a_publish_opens_one_card() {
    let (model, _script) = spawn_model(vec![
        rule(
            |c| c.is("Chief Executive") && c.fresh(),
            call(
                "delegate_to_desk",
                json!({ "desk": "content", "instruction": "Draft the Q3 board memo." }),
            ),
        ),
        rule(|c| c.is("Writer"), say("Here is the draft.")),
        rule(|c| c.is("Chief Executive"), say("The desk drafted it.")),
    ])
    .await;
    let host = spawn_host(model).await;

    let reply = host.chat("assemble the Q3 board pack", None).await;
    let board = host.board(&reply).await;
    let card = board.only();
    assert_eq!(card["assignee"].as_str(), Some("writer"));
    assert_eq!(board.linked.as_deref(), Some(id_of(card)));
}

/// A publish with no card in scope still mints one. This is #445's own case and
/// the reason the fix is "file onto the card when there is one" rather than
/// "never mint": a chat deliverable with nothing tracking it would otherwise go
/// back to being unreachable.
#[tokio::test(flavor = "multi_thread")]
async fn a_publish_with_no_card_in_scope_mints_one() {
    let (model, _script) = spawn_model(vec![
        rule(
            |c| c.is("Chief Executive") && c.fresh(),
            call(
                "publish_artifact",
                json!({ "path": "notes.md", "title": "Some notes" }),
            ),
        ),
        rule(|c| c.is("Chief Executive"), say("Published the notes.")),
    ])
    .await;
    let host = spawn_host(model).await;
    host.seed_file("ceo", "notes.md", "notes\n");

    // Neither trackable work nor a recognised imperative, so nothing else in
    // the turn can open a card — same property "hi" had before issue #1725's
    // greeting fast path. NOT "hi" itself: a bare greeting is now a matched
    // `Chatter` classification that runs the reduced-scope chat-only turn
    // (no tools offered at all), so the model could never reach the
    // `publish_artifact` call this test's whole point is to exercise. This
    // fixture is ambiguous `Chatter` by abstention instead (it opens with
    // neither a `SMALLTALK_OPENERS` greeting nor a work verb nor a `?`), which
    // keeps the turn's tools available exactly like every non-greeting
    // message did before #1725.
    //
    // And NOT "ok" either, which is what this branch reached for before the
    // merge: #1725 landed twice, by two mechanisms that stack. This branch adds
    // `small_talk`, which answers a bare hello or thanks from the cycle with no
    // turn at all; `main` already carried `is_matched_chatter` → the chat-only
    // turn hint, which fires on every *matched* `Chatter` — and `GREETINGS`
    // holds "ok". So "ok" reaches a turn, but a turn with no tools, and the
    // publish this test exists to exercise would never fire. Abstained
    // `Chatter` is the only class left that still gets tools, which is why the
    // fixture below is the one that survives both.
    let neutral = "the deck looks good to me";
    assert!(
        detect_task_intent(neutral).is_none(),
        "fixture must open no card via the REST handler, or this proves nothing"
    );
    let reply = host.chat(neutral, None).await;
    let board = host.board(&reply).await;
    assert_sole_card_carries(&host, &board, "ceo", "Some notes").await;
}

/// The constraint that stops the fix becoming its own bug: asking a question is
/// not commissioning work, on the orchestrator's thread or a desk's.
#[tokio::test(flavor = "multi_thread")]
async fn a_trivial_question_opens_no_card() {
    for desk in [None, Some("engineering")] {
        let (model, _script) = spawn_model(Vec::new()).await;
        let host = spawn_host(model).await;
        let reply = host.chat("what's the status of the build?", desk).await;
        let board = host.board(&reply).await;
        assert!(
            board.cards.is_empty(),
            "a question is not work ({desk:?}):{}",
            board.describe()
        );
    }
}

/// A bare acknowledgement in a desk thread that already has open work. The
/// open-work briefing the cycle appends made it long enough to score as
/// substantial, so the card-opening decision has to read the operator's own
/// words rather than the annotated message.
///
/// The seed request is worded to miss the REST detector on purpose — it has to
/// open a card **assigned to the engineer**, because that is what puts the
/// briefing on the next message. A seed the handler cards instead leaves an
/// unassigned To-do card, no briefing, and a regression check that passes
/// without exercising anything.
///
/// It was "thanks!" until issue #1725, which answers a bare greeting or thanks
/// from the runtime without a turn — so the briefing is never appended to one
/// and the vacuity guard below could no longer be satisfied. An acknowledgement
/// is deliberately outside that fast path ("yes" answering a teammate's
/// question is an instruction), so it still takes the annotated path this is
/// about, and the bug shape is unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn an_acknowledgement_in_a_desk_thread_with_open_work_opens_no_card() {
    let seed = "the nightly job keeps timing out — work out why and write up what you find";
    assert!(
        detect_task_intent(seed).is_none(),
        "the seed must open the DELEGATION card (assigned to the engineer), not the \
         handler's unassigned one, or there is no briefing and this proves nothing"
    );
    let (model, script) = spawn_model(vec![
        rule(
            |c| c.is("Chief Executive") && c.fresh(),
            call(
                "delegate_to_desk",
                json!({ "desk": "engineering", "instruction": "Investigate the flaky nightly job." }),
            ),
        ),
        rule(|c| c.is("Engineer"), say("Looking into it.")),
        rule(|c| c.is("Chief Executive"), say("Engineering is on it.")),
        rule(|c| c.is("Engineer"), say("Noted.")),
    ])
    .await;
    let host = spawn_host(model).await;

    let seeded = host.chat(seed, None).await;
    let before = host.board(&seeded).await;
    assert_eq!(
        before.only()["assignee"].as_str(),
        Some("engineer"),
        "the open work must belong to the engineer for the briefing to mention it"
    );

    let reply = host.chat("ok", Some("engineering")).await;
    let after = host.board(&reply).await;
    assert_eq!(
        after.cards.len(),
        before.cards.len(),
        "'ok' opened a card:{}",
        after.describe()
    );

    // …and the briefing really was on the message, or the check above is
    // vacuous: the bug needs the annotation present to reproduce.
    let engineer_saw = script.prompts_to("Engineer");
    assert!(
        engineer_saw
            .iter()
            .any(|p| p.contains("[Open work already handed to you")),
        "the open-work briefing was never appended, so nothing was exercised: {engineer_saw:?}"
    );
}

/// The per-turn delegation cap still holds: three hand-offs open three cards and
/// the fourth and fifth are refused in-turn rather than queued.
#[tokio::test(flavor = "multi_thread")]
async fn five_delegations_in_one_turn_open_three_cards() {
    let (model, _script) = spawn_model(vec![
        rule(
            |c| c.is("Chief Executive") && c.fresh(),
            Turn::Calls(vec![
                (
                    "delegate_to_desk".into(),
                    json!({ "desk": "content", "instruction": "Draft the memo." }),
                ),
                (
                    "delegate_to_desk".into(),
                    json!({ "desk": "research", "instruction": "Pull the numbers." }),
                ),
                (
                    "delegate_to_desk".into(),
                    json!({ "desk": "engineering", "instruction": "Check the pipeline." }),
                ),
                (
                    "delegate_to_desk".into(),
                    json!({ "desk": "content", "instruction": "Draft the press note." }),
                ),
                (
                    "delegate_to_desk".into(),
                    json!({ "desk": "research", "instruction": "Chart the churn." }),
                ),
            ]),
        ),
        rule(|c| c.is("Chief Executive"), say("Handed out the work.")),
    ])
    .await;
    let host = spawn_host(model).await;

    let reply = host
        .chat(
            "hand out these five pieces of work across the desks please",
            None,
        )
        .await;
    let board = host.board(&reply).await;
    assert_eq!(
        board.cards.len(),
        3,
        "three delegations open three cards, two are refused:{}",
        board.describe()
    );
}
