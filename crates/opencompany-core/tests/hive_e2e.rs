#![cfg(feature = "openhuman")]
//! **End-to-end: hive desks over the embedded OpenHuman runtime** (plan
//! hive-desks, Phase 8).
//!
//! The unit tests under `src/hive/` drive the episode host with a scripted
//! `SeatRunner`; they pin the fold and cannot tell you whether a *company*
//! runs a room: whether an operator message on a desk of two opens an
//! episode, whether each seat's turn goes through the ordinary harness turn
//! with its `opencompany` MCP server attached, whether the one speech tool
//! the seat calls lands as exactly one `AgentReply` with its episode
//! metadata, whether two desks sharing an agent run at once without that
//! agent ever running twice, and whether the journal that results is the
//! one `opencompany measure` and the console fold the same numbers from.
//!
//! So every test here boots a **real company** — `RuntimeBuilder`, the
//! process-wide `openhuman_embed::Runtime`, the filesystem store, the HTTP
//! surface, loopback magic-link sign-in — and drives it through
//! `POST /api/v1/company/chat`, the route the console posts to. Only the
//! model is scripted (`support::script_model`), and the script is the mock
//! brain's hive arm in Rust (`frontend/test/e2e/mock-brain.mjs`): it keys on
//! the episode brief's fence line -- `Every tool call must carry "chat":
//! "<deskId>" and "parent": <thread>.` -- and on what that brief shows the
//! seat, answers with the `desk_*` tool its own belt carries, and ends the
//! turn with a plain `stop` once it has recorded its part.
//!
//! It reads the room rather than counting turns, because a hosted seat's
//! session is cleared and reseeded from the journal every turn: there is no
//! transcript to count against, and a retried turn must answer the same way.
//!
//! # What each test proves
//!
//! | Test | Claim |
//! | --- | --- |
//! | `a_desk_answers_through_the_seat_its_routing_named` | the fallback plan names one seat, it records its part in one wave, and `EpisodeCompleted{complete_episode}` closes it |
//! | `a_broadcast_without_jev_falls_back_deterministically` | `BroadcastRouted{router: fallback}` names the desk lead, who is reopened in the next round |
//! | `an_ask_opens_a_conversation_the_desk_only_references` | the desk keeps `ConversationOpened` / `Concluded` naming the pair and its channel; the exchange itself is the ask row and what hangs off it |
//! | `a_single_member_desk_answers_with_one_ordinary_turn` | a desk of one is one reply with no episode frames |
//! | `a_cross_desk_referral_crosses_only_the_answer_back` | **ignored**: referral is not reconnected to the conductor |
//! | `a_shared_agent_on_two_desks_runs_both_rooms_without_running_twice` | `companies/hive_demo`: both episodes complete, brackets overlap across desks, never for the same agent |
//! | `a_checkpoint_replays_the_rows_after_it_as_a_no_op` | the round-0 checkpoint plus the rows after it fold to the final state; folding them again changes nothing |
//! | `a_desk_remembers_across_episodes_through_its_memory_tools` | `memory_store` on the seat's own belt in one episode, `memory_recall` in the next, the recorded part cites what came back |
//! | `a_seat_publishes_a_deliverable_the_operator_can_edit` | a seat's `publish_artifact` is filed on a card, its recorded part links the artifact, and an operator edit lands as version 2 |
//! | `a_turn_that_only_publishes_hands_over_on_a_row_of_its_own` | a turn that published and said nothing hands the artifact over on an outputs-only row when its wave ends |
//!
//! Every company gets a unique id: the runtime keeps one `Agent` per
//! `(company, agent)` for the life of the process, so two tests naming the
//! same company would share transcripts.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::script_model::{Ask, Reply, Responder, spawn_script_with_latency};

use opencompany::CompanyRuntime;
use opencompany::company::CompanyManifest;
use opencompany::hive::measure::{Report, Thresholds, measure};
use opencompany::hive::referral::HIVE_REFERRAL_AUTHOR;
use opencompany::hive::routing::Router;
use opencompany::ports::types::{
    CompanyEvent, CompanyId, EpisodeReason, EventSeq, StoredEvent, UtteranceKind,
};
use opencompany::runtime::RuntimeBuilder;
use opencompany::{AppConfig, AppState};

// ---------------------------------------------------------------------------
// The seat as the scripted model sees it
// ---------------------------------------------------------------------------

/// The line the episode brief ends every seat turn with, naming the desk
/// every tool call must carry. It is the one anchor that is always present
/// and always this turn's, which is what makes it the seat marker.
///
/// The old marker was a `Hive turn: desk …, episode …, round …` sentinel this
/// crate prepended itself. The hosted runner hands a seat `tinyhivemind`'s own
/// episode brief instead, which carries no such line -- and no episode id or
/// round number anywhere, because a seat never needed them: its belt is bound
/// to one seat of one episode, so the tools know.
const FENCE: &str = "Every tool call must carry \"chat\": \"";

/// The rest of that line, naming the thread a call must be made in. `null`
/// on the desk itself; a sequence inside a conversation an `ask` opened.
const FENCE_PARENT: &str = "and \"parent\": ";

/// The brief's heading over what the seat has not seen yet.
const DESK_MESSAGES: &str = "## New desk messages\n";

/// The speech tools a seat ends its turn with, as its belt names them.
///
/// Prefixed, because the episode's tools ride the seat's own belt now rather
/// than the `opencompany` MCP server, and this company prefixes them so the
/// episode's names cannot be admitted as its own (`hive::host::TOOL_PREFIX`).
/// `desk_read` is absent on purpose: reading is not speaking, and a turn that
/// only read has not recorded anything.
const SPEECH: [&str; 3] = ["desk_broadcast", "desk_ask", "desk_complete_episode"];

/// The roster role each teammate's persona announces, by id.
///
/// The brief names no speaker -- the belt is bound to one, so the model is
/// never told which it is -- and the persona opens `You are the <role> at
/// <company>`. That role is the only thing on the wire that says whose turn
/// this is, and it is this file's own manifest that gave it.
const ROLES: [(&str, &str); 4] = [
    (CEO, "Chief Executive"),
    (ENGINEER, "Engineer"),
    (WRITER, "Writer"),
    (GREETER, "Front desk"),
];

/// One seat turn, read off a request: the desk it runs on, who is speaking,
/// what the brief handed them, and the tool results this turn has collected.
#[derive(Clone, Debug)]
struct Seat {
    desk: String,
    /// The thread this turn speaks in: `None` on the desk, `Some(seq)`
    /// inside a conversation. Echoing it back is what keeps a conversation's
    /// answer on the conversation instead of on the desk root.
    parent: Option<u64>,
    speaker: String,
    /// Every teammate the brief attributes an unseen row to, in order. Empty
    /// on the opening wave, when the operator is the only voice.
    ///
    /// This is what a seat decides on, and it is why the script needs no
    /// turn counter: the room's state is in the brief, and a retried turn
    /// reads the same brief and answers the same way.
    heard: Vec<String>,
    /// Tool results this turn has already collected, oldest first.
    turn_tools: Vec<String>,
    /// The tools this turn already called, oldest first.
    calls: Vec<String>,
    /// The brief, verbatim.
    prompt: String,
    /// The `## New desk messages` block alone.
    assignment: String,
}

/// The desk named by the brief's fence line, which is the marker that says
/// this request is a seat turn at all.
fn fence_desk(text: &str) -> Option<String> {
    let at = text.rfind(FENCE)?;
    let rest = &text[at + FENCE.len()..];
    rest.split_once('"').map(|(desk, _)| desk.to_string())
}

/// The thread the fence line names, or `None` for the desk itself.
///
/// The desk spells it bare (`and "parent": null.`) and a conversation spells
/// it quoted (`and "parent": "14".`), so the quotes come off before the
/// parse. Getting this wrong is silent and expensive: the call goes out with
/// `parent: null`, the room refuses it with "this turn is in chat `x` with
/// parent `14`; name exactly those", and the seat is turned again to make
/// the same mistake until the wall.
fn fence_parent(text: &str) -> Option<u64> {
    let at = text.rfind(FENCE_PARENT)?;
    let rest = &text[at + FENCE_PARENT.len()..];
    let (value, _) = rest.split_once('.')?;
    value.trim().trim_matches('"').parse().ok()
}

/// The heading OpenHuman's writing-style block opens with.
const STYLE_HEADING: &str = "# Writing style";

/// The heading OpenHuman's grounding contract opens with.
const GROUNDING_HEADING: &str = "## Grounding and tool use";

/// The opening of the brief that tells every agent a person reads its words.
const READER_BRIEF: &str = "A person reads what you post";

/// Every system message on a request, joined.
fn system_messages(ask: &Ask) -> String {
    ask.messages
        .iter()
        .filter(|message| role(message) == "system")
        .map(content)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every seat request carries the grounding contract, the writing-style rules
/// and the reader brief exactly once, cold or seeded. Returns how many were
/// seeded.
fn every_seat_turn_is_grounded_and_styled(asks: &[Ask]) -> usize {
    let seat_asks: Vec<&Ask> = asks.iter().filter(|ask| seat_of(ask).is_some()).collect();
    assert!(!seat_asks.is_empty(), "no seat turns were captured");
    let mut seeded = 0;
    for ask in seat_asks {
        let system = system_messages(ask);
        if ask
            .messages
            .iter()
            .filter(|message| role(message) == "user")
            .count()
            > 1
        {
            seeded += 1;
        }
        for heading in [STYLE_HEADING, GROUNDING_HEADING, READER_BRIEF] {
            assert_eq!(
                system.matches(heading).count(),
                1,
                "a seat turn must carry `{heading}` exactly once: {system}"
            );
        }
    }
    seeded
}

fn role(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("")
}

fn content(message: &Value) -> &str {
    message.get("content").and_then(Value::as_str).unwrap_or("")
}

/// Whose turn this is, read from the persona the seat was built with.
///
/// Scans every message rather than the first `system` one. A hosted turn is
/// assembled from the host's own composed prompt, then any recalled
/// preamble, then the session's messages -- so the persona is not reliably
/// the first system message, and reading only that one made later turns
/// unparseable while the early ones worked.
fn speaker_of(messages: &[Value]) -> Option<String> {
    let role_text = messages.iter().rev().find_map(|message| {
        let (_, opening) = content(message).rsplit_once("You are the ")?;
        Some(opening.split_once(" at ")?.0.trim().to_string())
    })?;
    ROLES
        .iter()
        .find(|(_, title)| *title == role_text)
        .map(|(id, _)| (*id).to_string())
}

/// Every teammate the brief attributes a row to, oldest first.
///
/// `@operator` and `@system` are not seats: the first opened the episode and
/// the second is the conductor's own nudge.
fn heard_in(assignment: &str) -> Vec<String> {
    assignment
        .lines()
        .filter_map(|line| line.strip_prefix('@'))
        .filter_map(|line| line.split_once(':'))
        .map(|(who, _)| who.trim().to_string())
        .filter(|who| ROLES.iter().any(|(id, _)| id == who))
        .collect()
}

fn seat_of(ask: &Ask) -> Option<Seat> {
    let last_user = ask
        .messages
        .iter()
        .rposition(|message| role(message) == "user")?;
    let prompt = content(&ask.messages[last_user]).to_string();
    let desk = fence_desk(&prompt)?;
    let parent = fence_parent(&prompt);
    let speaker = speaker_of(&ask.messages)?;
    let assignment = prompt
        .rsplit_once(DESK_MESSAGES)
        .map(|(_, rest)| rest.split("\n\n").next().unwrap_or(rest).to_string())
        .unwrap_or_default();
    let after = &ask.messages[last_user + 1..];
    let turn_tools = after
        .iter()
        .filter(|message| role(message) == "tool")
        .map(|message| content(message).to_string())
        .collect();
    // The belt is native now, so a call is its own name — no `mcp_call_tool`
    // envelope to read a served name back out of.
    let calls = after
        .iter()
        .filter(|message| role(message) == "assistant")
        .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
        .flatten()
        .filter_map(|call| {
            let name = call.get("function")?.get("name")?.as_str()?;
            Some(name.to_string())
        })
        .collect();
    Some(Seat {
        desk,
        parent,
        speaker,
        heard: heard_in(&assignment),
        turn_tools,
        calls,
        prompt,
        assignment,
    })
}

impl Seat {
    /// Whether this turn is over.
    ///
    /// Two acts end one, for different reasons. `complete_episode` records
    /// the seat's part, which is what it was turned for. `ask` opens a
    /// conversation that runs on its own: the seat says what it needs, ends
    /// its turn, and the answer reaches it on a later one — it *cannot*
    /// finish until then, so carrying on would only stack up questions.
    ///
    /// A `broadcast` ends nothing. It hands another seat's work on and says
    /// nothing about the speaker's own, so a seat that only broadcast still
    /// holds open work and the room keeps turning to it.
    fn spoke(&self) -> bool {
        self.answered("desk_complete_episode") || self.called("desk_ask")
    }

    /// Whether `tool` was called and answered without refusal this turn.
    ///
    /// Used where a refused call must not count — a completion the room
    /// rejected has not ended anything.
    fn answered(&self, tool: &str) -> bool {
        self.calls
            .iter()
            .zip(self.turn_tools.iter())
            .any(|(call, output)| call == tool && !is_refused(output))
    }

    /// Whether `tool` was called this turn at all.
    ///
    /// Deliberately blind to refusal, unlike [`answered`](Self::answered):
    /// [`is_refused`] is a word search, and the room's own contract language
    /// for an `ask` ("you will not be able to finish until…") trips it. A
    /// guard against repeating a call must not depend on that.
    fn called(&self, tool: &str) -> bool {
        self.calls.iter().any(|call| call == tool)
    }

    /// The operator's own words, when the brief carries them.
    ///
    /// The **last** such row, not the first: a brief lists everything the
    /// seat has not seen, so a second episode on a desk that already had one
    /// shows both messages and the turn is about the newer.
    fn operator_asked(&self) -> &str {
        self.assignment
            .lines()
            .filter_map(|line| line.strip_prefix("@operator: "))
            .next_back()
            .unwrap_or_default()
    }

    /// The result of the last non-speech tool this turn called, when it
    /// answered.
    fn last_tool_result(&self) -> Option<&str> {
        self.calls
            .iter()
            .zip(self.turn_tools.iter())
            .filter(|(call, _)| !SPEECH.contains(&call.as_str()))
            .map(|(_, output)| output.as_str())
            .next_back()
    }
}

/// Whether a tool result reads as the host refusing the call.
fn is_refused(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "refused",
        "error",
        "rejected",
        "invalid",
        "not a member",
        "cannot",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// One speech act, as the seat's own belt carries it.
///
/// Every call carries the desk and thread the fence named, which the room
/// refuses a call without.
fn speech(tool: &str, seat: &Seat, mut arguments: Value) -> Reply {
    if let Some(object) = arguments.as_object_mut() {
        object.insert("chat".to_string(), json!(seat.desk));
        // A **string**, not a number: the schema is `string | null`, and the
        // fence quotes it for the same reason. Sending the integer is
        // refused with "arguments.parent must be one of string, null, got
        // integer", the seat is turned again to make the same mistake, and
        // the conversation times out at the wall having never been answered.
        object.insert(
            "parent".to_string(),
            seat.parent
                .map_or(Value::Null, |seq| json!(seq.to_string())),
        );
    }
    Reply::Call {
        tool: Box::leak(format!("desk_{tool}").into_boxed_str()),
        args: arguments,
    }
}

fn broadcast(seat: &Seat, message: impl Into<String>) -> Reply {
    speech("broadcast", seat, json!({ "message": message.into() }))
}

fn complete(seat: &Seat, message: impl Into<String>) -> Reply {
    speech(
        "complete_episode",
        seat,
        json!({ "message": message.into() }),
    )
}

/// The turn is over once the seat's one speech act is recorded.
const DONE: &str = "done";

/// A script over seats: `act` decides the speech act for a seat turn that
/// has not spoken yet; a turn that has spoken stops; a request that is no
/// seat turn at all is answered with `plain`.
fn seat_script(
    plain: &'static str,
    act: impl Fn(&Seat) -> Reply + Send + Sync + 'static,
) -> Responder {
    Arc::new(move |ask: &Ask| {
        let Some(seat) = seat_of(ask) else {
            if std::env::var_os("HIVE_E2E_TURNS").is_some() {
                let last_user = ask
                    .messages
                    .iter()
                    .rev()
                    .find(|m| role(m) == "user")
                    .map(|m| content(m).to_string())
                    .unwrap_or_default();
                eprintln!(
                    "[unparsed] pending={:?} desk={:?} speaker={:?} roles={:?} tail={:?}",
                    ask.pending_tool,
                    fence_desk(&last_user),
                    speaker_of(&ask.messages),
                    ask.messages
                        .iter()
                        .map(|m| role(m).to_string())
                        .collect::<Vec<_>>(),
                    last_user.chars().rev().take(120).collect::<String>()
                );
            }
            if ask.pending_tool.is_some() {
                return Reply::Say(DONE.to_string());
            }
            return Reply::Say(plain.to_string());
        };
        // `HIVE_E2E_TURNS=1` prints what each seat turn saw and did. A
        // hosted seat's session is cleared every turn, so this is the only
        // place the two are visible together.
        if std::env::var_os("HIVE_E2E_TURNS").is_some() {
            eprintln!(
                "[turn] {} parent={:?} heard={:?} calls={:?} results={:?}",
                seat.speaker, seat.parent, seat.heard, seat.calls, seat.turn_tools
            );
        }
        if seat.spoke() {
            return Reply::Say(DONE.to_string());
        }
        act(&seat)
    })
}

/// The mock brain's own hive arm: record this seat's part.
///
/// Recording *is* a seat's contribution to a completion episode — the brief
/// says so in as many words — so the ordinary act is one
/// `complete_episode`, and a desk whose seats all record folds.
///
/// Handing work on is a different act with different consequences, and it is
/// deliberately not the default: a `broadcast` reopens whoever it routes to,
/// and two seats each handing the same opening work to the other invalidates
/// both their completions and the room never settles. The test that is about
/// hand-off scripts it explicitly.
fn record_part(seat: &Seat) -> Reply {
    complete(
        seat,
        format!(
            "desk {} by @{}: done, nothing left open.",
            seat.desk, seat.speaker
        ),
    )
}

/// Hand the opening work on once, then record — the hand-off path.
///
/// The operator's row is in a seat's brief only until it has seen it, so
/// "the operator is still unread and I have not broadcast yet" is the one
/// turn that opens. Everything after records.
fn hand_off_then_record(seat: &Seat) -> Reply {
    if !seat.operator_asked().is_empty() && !seat.called("desk_broadcast") {
        return broadcast(
            seat,
            format!("desk {} by @{}: opening hand-off.", seat.desk, seat.speaker),
        );
    }
    record_part(seat)
}

// ---------------------------------------------------------------------------
// The company
// ---------------------------------------------------------------------------

const ENGINEERING: &str = "engineering";
const CONTENT: &str = "content";
const FRONT: &str = "front";
const ENGINEER: &str = "engineer";
const WRITER: &str = "writer";
const CEO: &str = "ceo";
const GREETER: &str = "greeter";
const ADMIN: &str = "operator@opencompany.local";
/// The admin `companies/hive_demo` grants.
const DEMO_ADMIN: &str = "harness-e2e@tinyhumans.ai";

/// Two desks of two sharing the CEO — `companies/hive_demo`'s shape — plus a
/// desk of one, so one company covers every surface these tests need.
///
/// `[policy] mode = "full"` so no tool call parks: this file is about the
/// room, and a parked turn would hang an episode rather than fail it.
fn manifest(name: &str, base_url: &str) -> String {
    format!(
        r#"
[company]
name = "{name}"
summary = "Proves desks answer as rooms."

[inference]
provider = "ollama"
base_url = "{base_url}"

# Every tier the roster reaches: the CEO is an orchestrator and asks for
# `agentic-v1`; an unmapped tier is refused, not passed through.
[inference.models]
chat-v1 = "llama3"
reasoning-v1 = "llama3"
agentic-v1 = "llama3"

[policy]
mode = "full"

[tools]
allow = []

[users]
admins = ["{ADMIN}"]

[[agent]]
id = "{CEO}"
role = "Chief Executive"
tier = "orchestrator"

[[agent]]
id = "{ENGINEER}"
role = "Engineer"

[[agent]]
id = "{WRITER}"
role = "Writer"

[[agent]]
id = "{GREETER}"
role = "Front desk"

[[group_chat]]
id = "{ENGINEERING}"
name = "Engineering"
description = "How things are built."
members = ["{ENGINEER}", "{CEO}"]

[group_chat.routing]
round_width = 2

[group_chat.routing.referral]
enabled = true
max_hops = 1
returns = true

[[group_chat]]
id = "{CONTENT}"
name = "Content"
description = "Written drafts and copy."
members = ["{WRITER}", "{CEO}"]

[group_chat.routing]
round_width = 2

[group_chat.routing.referral]
enabled = true
max_hops = 1
returns = true

[[group_chat]]
id = "{FRONT}"
name = "Front"
members = ["{GREETER}"]
"#
    )
}

/// A company id no other test in this process uses.
fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// Boots `manifest` on loopback under a unique company id.
async fn boot(
    home: &Path,
    company_id: &str,
    mut manifest: CompanyManifest,
) -> (SocketAddr, Arc<CompanyRuntime>) {
    manifest.apply_globals();
    let problems = manifest.validate();
    assert!(problems.is_empty(), "the manifest is valid: {problems:?}");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let company_id = CompanyId::new(company_id);
    let state = AppState::new(AppConfig {
        bind: address.to_string(),
        ..AppConfig::default()
    })
    .with_home(home.to_path_buf());
    let runtime = Arc::new(
        RuntimeBuilder::new(state.home().to_path_buf(), manifest)
            .with_id(company_id.clone())
            .with_harness(Arc::new(opencompany::harness::HarnessPool::new()))
            .build()
            .await
            .expect("the company builds"),
    );
    state
        .registry()
        .insert(company_id.clone(), Arc::clone(&runtime));
    tokio::spawn(async move {
        let _ = opencompany::server::serve_on(listener, state).await;
    });
    (address, runtime)
}

/// Boots the in-test company.
async fn boot_lab(home: &Path, base_url: &str) -> (SocketAddr, Arc<CompanyRuntime>) {
    let id = unique("hive-lab");
    let manifest = CompanyManifest::from_stored_toml(&manifest(&id, base_url))
        .expect("the in-test manifest parses");
    boot(home, &id, manifest).await
}

/// Boots `companies/hive_demo` — the measurement's company — against the
/// scripted model.
async fn boot_demo(home: &Path, base_url: &str) -> (SocketAddr, Arc<CompanyRuntime>) {
    let bundle = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../companies/hive_demo");
    let mut manifest = CompanyManifest::from_path(&bundle).expect("companies/hive_demo parses");
    manifest.inference.provider = Some("ollama".into());
    manifest.inference.base_url = Some(base_url.into());
    for tier in ["chat-v1", "reasoning-v1", "agentic-v1"] {
        manifest
            .inference
            .models
            .insert(tier.into(), "llama3".into());
    }
    boot(home, &unique("hive-demo"), manifest).await
}

// ---------------------------------------------------------------------------
// The operator
// ---------------------------------------------------------------------------

/// A cookie-carrying HTTP client — `reqwest`'s own cookie store is behind a
/// feature this crate does not enable.
struct Client {
    inner: reqwest::Client,
    base: String,
    cookie: Mutex<Option<String>>,
}

impl Client {
    fn new(address: SocketAddr) -> Self {
        Self {
            inner: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
            base: format!("http://{address}"),
            cookie: Mutex::new(None),
        }
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let mut request = self.inner.post(format!("{}{path}", self.base)).json(&body);
        if let Some(cookie) = self.cookie.lock().unwrap().clone() {
            request = request.header(reqwest::header::COOKIE, cookie);
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
        let mut request = self.inner.get(format!("{}{path}", self.base));
        if let Some(cookie) = self.cookie.lock().unwrap().clone() {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        let response = request.send().await.expect("the loopback host answers");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let json = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, json)
    }

    /// Signs in over the loopback magic-link flow, which echoes the code.
    async fn sign_in(&self, email: &str) {
        let (status, body) = self
            .post("/api/v1/company/auth/request", json!({ "email": email }))
            .await;
        assert_eq!(status, 200, "sign-in refused: {body}");
        let code = body["dev_code"]
            .as_str()
            .unwrap_or_else(|| panic!("no dev_code, so no session: {body}"))
            .to_string();
        let (status, body) = self
            .post("/api/v1/company/auth/verify", json!({ "code": code }))
            .await;
        assert_eq!(status, 200, "the login code was refused: {body}");
    }

    /// Posts one operator message to `desk`. A desk with a room answers
    /// through its episode, so this returns as soon as the message is
    /// journaled; a desk of one answers in the response.
    async fn say(&self, desk: &str, text: &str) -> Value {
        let (status, body) = self
            .post(
                "/api/v1/company/chat",
                json!({ "text": text, "chat": desk }),
            )
            .await;
        assert_eq!(status, 200, "chat refused: {body}");
        body
    }
}

// ---------------------------------------------------------------------------
// Reading the journal back
// ---------------------------------------------------------------------------

async fn journal(runtime: &Arc<CompanyRuntime>) -> Vec<StoredEvent> {
    runtime
        .events()
        .read_from(runtime.id(), EventSeq::new(0), 100_000)
        .await
        .expect("the journal reads back")
}

/// With `HIVE_E2E_DUMP` set, prints the journal and every request the
/// scripted model saw — the way to read a failing run.
fn dump(rows: &[StoredEvent], script: &support::script_model::Script) {
    if std::env::var_os("HIVE_E2E_DUMP").is_none() {
        return;
    }
    for row in rows {
        eprintln!(
            "[journal] {} {}",
            row.seq.value(),
            serde_json::to_string(&row.event)
                .unwrap_or_default()
                .chars()
                .take(400)
                .collect::<String>()
        );
    }
    for ask in script.asks() {
        eprintln!(
            "[ask] tools={:?} seat={:?} pending={:?}",
            ask.tools,
            seat_of(&ask).map(|seat| (
                seat.speaker,
                seat.parent,
                seat.heard,
                seat.assignment,
                seat.calls
            )),
            ask.pending_tool
        );
    }
}

/// Polls the journal until `done` holds, or fails after `timeout`.
async fn wait_for(
    runtime: &Arc<CompanyRuntime>,
    what: &str,
    timeout: Duration,
    done: impl Fn(&[StoredEvent]) -> bool,
) -> Vec<StoredEvent> {
    let started = Instant::now();
    loop {
        let rows = journal(runtime).await;
        if done(&rows) {
            if std::env::var_os("HIVE_E2E_DUMP").is_some() {
                for row in &rows {
                    eprintln!(
                        "[journal] {} {}",
                        row.seq.value(),
                        serde_json::to_string(&row.event)
                            .unwrap_or_default()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    );
                }
            }
            return rows;
        }
        if started.elapsed() >= timeout {
            if std::env::var_os("HIVE_E2E_DUMP").is_some() {
                for row in &rows {
                    eprintln!(
                        "[journal] {} {}",
                        row.seq.value(),
                        serde_json::to_string(&row.event)
                            .unwrap_or_default()
                            .chars()
                            .take(600)
                            .collect::<String>()
                    );
                }
            }
            panic!(
                "timed out waiting for {what}; journal kinds: {:?}",
                rows.iter().map(|row| row.event.kind()).collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Every `EpisodeCompleted`, as `(episode_id, chat_id, reason, rounds)`.
fn completions(rows: &[StoredEvent]) -> Vec<(String, String, EpisodeReason, u32)> {
    rows.iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::EpisodeCompleted {
                episode_id,
                chat_id,
                reason,
                rounds,
                ..
            } => Some((episode_id.clone(), chat_id.clone(), *reason, *rounds)),
            _ => None,
        })
        .collect()
}

/// `n` episodes have completed, and no seat bracket is still open.
fn completed(n: usize) -> impl Fn(&[StoredEvent]) -> bool {
    move |rows| {
        completions(rows).len() >= n && {
            let report = opencompany::hive::measure::measure_rows(
                &CompanyId::new("x"),
                EventSeq::new(0),
                rows,
            );
            report.open_turns == 0
        }
    }
}

/// One journaled reply.
#[derive(Clone, Debug)]
struct ReplyRow {
    seq: u64,
    agent: String,
    text: String,
    audience: Vec<String>,
    kind: Option<UtteranceKind>,
    to: Vec<String>,
    episode: Option<String>,
}

fn replies(rows: &[StoredEvent], chat: &str) -> Vec<ReplyRow> {
    rows.iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::AgentReply {
                chat_id,
                agent_id,
                text,
                audience,
                episode,
                ..
            } if chat_id == chat => Some(ReplyRow {
                seq: row.seq.value(),
                agent: agent_id.clone(),
                text: text.clone(),
                audience: audience.clone(),
                kind: episode.as_ref().map(|episode| episode.kind),
                to: episode
                    .as_ref()
                    .map(|episode| episode.to.clone())
                    .unwrap_or_default(),
                episode: episode.as_ref().map(|episode| episode.id.clone()),
            }),
            _ => None,
        })
        .collect()
}

/// Every `RoundStarted` on `chat`, as `(revision, seats)`.
fn rounds(rows: &[StoredEvent], chat: &str) -> Vec<(u64, Vec<String>)> {
    let mut waves: std::collections::BTreeMap<u64, Vec<String>> = std::collections::BTreeMap::new();
    for row in rows {
        let CompanyEvent::TurnStarted {
            chat_id,
            agent_id: Some(agent_id),
            round_revision: Some(revision),
            ..
        } = &row.event
        else {
            continue;
        };
        if chat_id != chat {
            continue;
        }
        let seats = waves.entry(*revision).or_default();
        if !seats.contains(agent_id) {
            seats.push(agent_id.clone());
        }
    }
    waves.into_iter().collect()
}

/// The coordination report over the company's whole journal — what
/// `opencompany measure --company <id>` prints.
async fn report(runtime: &Arc<CompanyRuntime>) -> Report {
    measure(runtime.events().as_ref(), runtime.id(), EventSeq::new(0))
        .await
        .expect("the journal measures")
}

/// A generous bound: every seat turn here is a couple of loopback calls.
const EPISODE: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// 1: a desk answers through the seat its routing named
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_desk_answers_through_the_seat_its_routing_named() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted.", record_part),
        Duration::from_millis(150),
    )
    .await;
    let (address, runtime) = boot_lab(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    let accepted = client
        .say(
            ENGINEERING,
            "Plan the staging rollout for the new checkout.",
        )
        .await;
    assert!(
        accepted["responses"].is_array(),
        "the message is accepted: {accepted}"
    );

    let rows = wait_for(&runtime, "the episode to complete", EPISODE, completed(1)).await;

    // No Jev credential, so routing falls back — and a fallback plan names
    // one seat. The desk's `round_width` does not widen that: it bounds how
    // many recipients a broadcast may be placed to, not how many seats open
    // an episode. A desk of two answering through one of them is the routing
    // plan's answer, not a shortfall.
    let opened: Vec<(String, Vec<String>, Router)> = rows
        .iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::EpisodeOpened {
                chat_id,
                participants,
                plan,
                ..
            } => Some((chat_id.clone(), participants.clone(), plan.router())),
            _ => None,
        })
        .collect();
    assert_eq!(
        opened,
        vec![(
            ENGINEERING.to_string(),
            vec![ENGINEER.to_string()],
            Router::Fallback
        )],
        "the fallback plan names the seat that answers"
    );

    // One wave: the seat was due, it recorded its part, and nothing else was
    // owed a turn. The wave is folded from the turn rows, because the
    // conductor announces no round — a wave is whoever is due, decided as it
    // goes, so who was in one is only knowable from who turned in it.
    assert_eq!(
        rounds(&rows, ENGINEERING),
        vec![(0, vec![ENGINEER.to_string()])],
        "one wave, the routed seat in it"
    );

    let done = completions(&rows);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].1, ENGINEERING);
    assert_eq!(done[0].2, EpisodeReason::CompleteEpisode);
    assert_eq!(done[0].3, 1, "one wave ran");

    // **The episode wrote down what a restart would otherwise lose.**
    //
    // Every wave settles with a checkpoint: the seats, their watermarks, the
    // ledger of outstanding asks, the conversations open under it. None of
    // that is a row, so without this it lives in memory and dies with the
    // process. Asserted by reading it back the way a resume would, and
    // deserializing it into the type the conductor resumes from -- a
    // checkpoint that will not round-trip is one a restart cannot use.
    let checkpoint = opencompany::hive::episode_store::latest_state(
        runtime.events().as_ref(),
        runtime.id(),
        &done[0].0,
    )
    .await
    .expect("the journal reads")
    .expect("the episode checkpointed itself");
    assert_eq!(checkpoint.desk, ENGINEERING);
    assert!(
        checkpoint.sharing.is_empty(),
        "the conductor keeps its watermarks in `state`: {:?}",
        checkpoint.sharing
    );
    let resumable: tinyhivemind_driver::ConductorState =
        serde_json::from_value(checkpoint.state.clone())
            .expect("the snapshot is what the conductor resumes from");
    assert_eq!(
        resumable.chat, ENGINEERING,
        "the snapshot names the desk it ran on"
    );

    // Recording *is* a seat's contribution: there is no separate "say
    // something" act in a completion episode, so one seat leaves one row.
    let desk = replies(&rows, ENGINEERING);
    let kinds: Vec<(String, Option<UtteranceKind>)> = desk
        .iter()
        .map(|row| (row.agent.clone(), row.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![(ENGINEER.to_string(), Some(UtteranceKind::CompleteEpisode))],
        "the seat's recorded part is its reply: {desk:?}"
    );
    assert!(
        desk.iter()
            .all(|row| row.episode == Some(done[0].0.clone())),
        "every row names the episode: {desk:?}"
    );
    assert!(
        desk.iter().all(|row| row.text.contains("by @")),
        "the row text is the tool's `message`, verbatim: {desk:?}"
    );

    // The seat was handed the operator's words, and its own belt to answer
    // with — the episode's tools ride the session now, not the MCP server.
    let seats: Vec<Seat> = script.asks().iter().filter_map(seat_of).collect();
    let opener = seats
        .iter()
        .find(|seat| seat.speaker == ENGINEER && seat.turn_tools.is_empty())
        .expect("the engineer's turn");
    assert_eq!(
        opener.operator_asked(),
        "Plan the staging rollout for the new checkout."
    );
    assert!(
        opener.parent.is_none(),
        "an opening turn speaks on the desk, not in a conversation"
    );

    let measured = report(&runtime).await;
    assert_eq!(measured.episodes_completed, 1);
    assert_eq!(measured.same_agent_overlaps, 0);
    assert_eq!(
        measured.episodes[&done[0].0].rounds, 1,
        "the measure counts the wave the turn rows carry: {measured:?}"
    );
    assert_eq!(measured.utterance_kinds["complete_episode"], 1);
    assert!(
        !measured.utterance_kinds.contains_key("post"),
        "`post` is not served to a seat: {measured:?}"
    );

    // The console's episode list agrees with the journal.
    let (status, episodes) = client.get("/api/v1/company/episodes").await;
    assert_eq!(status, 200, "{episodes}");
    assert_eq!(episodes[0]["id"], done[0].0);
    assert_eq!(episodes[0]["status"], "completed");
    assert_eq!(episodes[0]["reason"], "complete_episode");

    // Every seat turn is attributable to its episode and its wave.
    //
    // Asserted on the turn rows, not on `GET /runs`: a seat turn opens no
    // run record at all under the conductor. The hand-written loop minted
    // one per seat (`NewRun::in_episode`, which nothing calls any more), so
    // the console's run list shows the operator's turn and nothing the desk
    // did. That is a gap, not a decision, and it is the last of the stamps
    // that went out with the old loop.
    let attributed: Vec<(String, u64)> = rows
        .iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::TurnStarted {
                agent_id: Some(agent),
                episode_id: Some(episode),
                round_revision: Some(revision),
                ..
            } if *episode == done[0].0 => Some((agent.clone(), *revision)),
            _ => None,
        })
        .collect();
    assert_eq!(
        attributed,
        vec![(ENGINEER.to_string(), 0)],
        "the seat turn names its episode and its wave"
    );
}

// ---------------------------------------------------------------------------
// 2: a broadcast without Jev falls back to the desk lead
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broadcast_without_jev_falls_back_deterministically() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        // This is the hand-off test, so this is the script that hands off:
        // the engineer broadcasts its opening work and then records, and
        // everyone else just records.
        seat_script("Noted.", |seat| {
            if seat.speaker == ENGINEER {
                return hand_off_then_record(seat);
            }
            record_part(seat)
        }),
        Duration::from_millis(50),
    )
    .await;
    let (address, runtime) = boot_lab(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    client.say(ENGINEERING, "Size the rollout.").await;
    let rows = wait_for(&runtime, "the episode to complete", EPISODE, completed(1)).await;

    let routed: Vec<(String, u64, Router, Vec<String>, u64)> = rows
        .iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::BroadcastRouted {
                agent_id,
                revision,
                router,
                plan,
                message_seq,
                ..
            } => Some((
                agent_id.clone(),
                *revision,
                *router,
                plan.agent_ids(),
                *message_seq,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(routed.len(), 1, "{routed:?}");
    let (by, _revision, router, targets, message_seq) = &routed[0];
    assert_eq!(by, ENGINEER, "the seat the fallback opened with broadcasts");
    assert_eq!(*router, Router::Fallback, "no TinyHumans key, no Jev");
    assert_eq!(
        targets,
        &vec![CEO.to_string()],
        "with no router to ask, the hand-off goes to the desk's other seat"
    );
    let desk = replies(&rows, ENGINEERING);
    let broadcast = desk
        .iter()
        .find(|row| row.seq == *message_seq)
        .expect("the routed row is the broadcast's reply");
    assert_eq!(broadcast.kind, Some(UtteranceKind::Broadcast));
    assert_eq!(broadcast.agent, ENGINEER);

    // The seat it was placed on is brought in: a later wave runs the CEO,
    // whose prompt says who handed it what.
    assert!(
        rounds(&rows, ENGINEERING)
            .iter()
            .any(|(rev, seats)| *rev > 0 && seats.contains(&CEO.to_string())),
        "the hand-off brings the other seat in: {:?}",
        rounds(&rows, ENGINEERING)
    );
    // The recipient is assigned *from the broadcast row itself*, and shown
    // it attributed to its author. Not a separate "handoff from @…" note:
    // that wording is for a hand-off queued for a seat that was busy, and
    // this one was placed directly.
    let told: Vec<String> = script
        .asks()
        .iter()
        .filter_map(seat_of)
        .filter(|seat| seat.speaker == CEO)
        .map(|seat| seat.prompt.clone())
        .collect();
    assert!(
        told.iter().any(|prompt| {
            prompt.contains(&format!("assignment was made at sequence {message_seq}"))
                && prompt.contains(&format!("@{ENGINEER}: "))
        }),
        "the CEO was not assigned from the engineer's hand-off: {told:?}"
    );
    every_seat_turn_is_grounded_and_styled(&script.asks());
    let done = completions(&rows);
    assert_eq!(done[0].2, EpisodeReason::CompleteEpisode, "{done:?}");
    let measured = report(&runtime).await;
    assert_eq!(measured.broadcasts, 1);
    assert_eq!(measured.routers["fallback"], 1);
    assert!(
        measured.distinct_pairs.contains("engineer→ceo"),
        "the contact runs from the seat that handed off to the one that took \
         it: {measured:?}"
    );
}

// ---------------------------------------------------------------------------
// 3: an ask opens a conversation the desk only references
// ---------------------------------------------------------------------------

/// The conductor's private act: one seat asks another, the exchange runs on
/// its own, and the desk keeps a reference to it rather than the exchange.
///
/// `dm` is not served to a seat at all — the library serves `broadcast`,
/// `ask`, `complete_episode` and `read` — so this is the shape a private
/// message actually takes on a desk now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ask_opens_a_conversation_the_desk_only_references() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted.", |seat| {
            // `!called` is load-bearing: a turn ends when the seat has
            // *recorded* its part, and an `ask` is not that. Without the
            // guard the same question is asked again on every continuation
            // of the turn, each opening its own conversation.
            if seat.speaker == ENGINEER
                && !seat.operator_asked().is_empty()
                && !seat.called("desk_ask")
            {
                return speech(
                    "ask",
                    seat,
                    json!({ "to": CEO, "message": "Between us: does the rollout need a freeze?" }),
                );
            }
            // Inside the conversation, the CEO hands a piece of the work to
            // the room before answering. That broadcast is the room's, not
            // the pair's -- the library lands it on the desk with no thread,
            // and it is the one row that carries a conversation marker while
            // belonging to the desk.
            if seat.speaker == CEO && seat.parent.is_some() && !seat.called("desk_broadcast") {
                return broadcast(seat, "someone should size the copy changes");
            }
            record_part(seat)
        }),
        Duration::from_millis(50),
    )
    .await;
    let (address, runtime) = boot_lab(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    client.say(ENGINEERING, "Decide on the freeze.").await;
    let rows = wait_for(&runtime, "the episode to complete", EPISODE, completed(1)).await;

    // The reference rows: opened and concluded, both on the desk, both
    // naming the pair and the channel the exchange itself went to.
    let opened: Vec<(String, String, String, u64)> = rows
        .iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::ConversationOpened {
                chat_id,
                conversation_id,
                asker,
                askee,
                root,
                ..
            } => Some((
                chat_id.clone(),
                conversation_id.clone(),
                format!("{asker}->{askee}"),
                *root,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        opened.len(),
        1,
        "one conversation, one reference: {opened:?}"
    );
    let (desk, conversation_id, pair, root) = &opened[0];
    assert_eq!(desk, ENGINEERING, "the reference sits on the desk");
    assert_eq!(pair, &format!("{ENGINEER}->{CEO}"));
    assert_eq!(
        conversation_id,
        &opencompany::hive::referral::pair_conversation(ENGINEER, CEO),
        "and points at the pair's own channel"
    );

    let concluded: Vec<(String, u64, bool)> = rows
        .iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::ConversationConcluded {
                conversation_id,
                root,
                forced,
                ..
            } => Some((conversation_id.clone(), *root, *forced)),
            _ => None,
        })
        .collect();
    assert_eq!(
        concluded,
        vec![(conversation_id.clone(), *root, false)],
        "it concluded on the same root, with an answer"
    );

    // The exchange itself is in the pair channel: the question, and the
    // answer hanging off it. That pair is what a reader reassembles it from.
    let aside = replies(&rows, conversation_id);
    let ask = aside
        .iter()
        .find(|row| row.kind == Some(UtteranceKind::Ask))
        .expect("the ask row, in the pair channel");
    assert_eq!(ask.agent, ENGINEER);
    assert_eq!(ask.to, vec![CEO.to_string()], "it names who it asked");
    assert_eq!(
        ask.audience,
        vec![CEO.to_string()],
        "and only the two of them read it"
    );
    assert_eq!(*root, ask.seq, "the reference is rooted at the ask row");
    assert!(
        aside
            .iter()
            .any(|row| row.kind == Some(UtteranceKind::CompleteEpisode)),
        "the answer is filed beside the question: {aside:?}"
    );

    // And the desk carries none of it. This is the whole point: the room's
    // timeline stays the room's, and the reference row is how it says an
    // exchange happened.
    let desk_rows = replies(&rows, ENGINEERING);
    assert!(
        desk_rows
            .iter()
            .all(|row| row.kind != Some(UtteranceKind::Ask)),
        "no part of the exchange is on the desk: {desk_rows:?}"
    );
    assert!(
        desk_rows.iter().all(|row| row.seq != ask.seq),
        "not even the question that opened it: {desk_rows:?}"
    );

    // The trap, asserted rather than trusted: work handed to the room from
    // inside the conversation lands on the *desk*. It carries a conversation
    // marker and no thread, so routing on the marker would file public work
    // where only two seats could read it, and nothing would say so.
    assert!(
        desk_rows
            .iter()
            .any(|row| row.kind == Some(UtteranceKind::Broadcast) && row.agent == CEO),
        "the hand-off reached the room: {desk_rows:?}"
    );
    assert!(
        aside
            .iter()
            .all(|row| row.kind != Some(UtteranceKind::Broadcast)),
        "and did not end up in the pair channel: {aside:?}"
    );

    // The askee answered inside the conversation: an ordinary turn, on the
    // thread the fence named, not on the desk.
    let answered = script
        .asks()
        .iter()
        .filter_map(seat_of)
        .any(|seat| seat.speaker == CEO && seat.parent == Some(ask.seq));
    assert!(answered, "the CEO was turned inside the conversation");

    // **A seat is never offered a hand-off tool it cannot use here.**
    //
    // `spawn_task`, `delegate_to_desk` and `delegate_to_teammate` are wired
    // onto every roster agent and queue work the brain drains; no brain
    // drains inside an episode, so the orchestrator refuses them outright
    // (`drain_unwired`). On a live run a seat reached for one, took the
    // refusal as proof that delegating was impossible, and told the operator
    // to go and make "the board" available -- while `ask`, the tool that
    // does work here, was on the same belt. The refusal was handled; the
    // misdiagnosis it invited was not, so the names come off the belt.
    let offered: Vec<String> = script
        .asks()
        .iter()
        .flat_map(|ask| ask.tools.clone())
        .collect();
    assert!(
        !offered.is_empty(),
        "the fixture saw no tool schemas at all, so this asserts nothing",
    );
    for withheld in ["spawn_task", "delegate_to_desk", "delegate_to_teammate"] {
        assert!(
            !offered.iter().any(|name| name == withheld),
            "`{withheld}` was offered to an episode seat: {offered:?}",
        );
    }

    let seated: Vec<String> = script
        .asks()
        .iter()
        .filter(|ask| seat_of(ask).is_some())
        .flat_map(|ask| ask.tools.clone())
        .collect();
    assert!(
        seated.iter().any(|name| name == "desk_read"),
        "a seat reads the room with the episode's own verb: {seated:?}",
    );
    for absent in ["read", "mcp_call_tool", "mcp_list_tools"] {
        assert!(
            !seated.iter().any(|name| name == absent),
            "`{absent}` was offered to an episode seat: {seated:?}",
        );
    }

    // **And the prose that describes them goes too.**
    //
    // Taking the tools without the briefs is worse than taking neither: the
    // persona still spends a paragraph on handing work on and on the board
    // tracking it, and a seat that goes looking for either finds nothing and
    // reports the capability as withdrawn rather than reaching for `ask`.
    // `tinyhivemind` runs this room; the orchestrator runtime's prose
    // describes a different one.
    let prompts = script
        .asks()
        .iter()
        .filter_map(seat_of)
        .map(|seat| seat.prompt)
        .collect::<Vec<_>>();
    assert!(!prompts.is_empty(), "no seat prompts were captured");
    for phrase in ["delegate_to_teammate", "Handing work on"] {
        assert!(
            !prompts.iter().any(|prompt| prompt.contains(phrase)),
            "an episode seat's prompt still describes the orchestrator runtime ({phrase:?})",
        );
    }

    assert!(
        every_seat_turn_is_grounded_and_styled(&script.asks()) > 0,
        "the asker's return is a seeded turn, which is the case this pins"
    );
    assert_eq!(completions(&rows)[0].2, EpisodeReason::CompleteEpisode);
}

// ---------------------------------------------------------------------------
// 4: a desk of one is one ordinary reply
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_member_desk_answers_with_one_ordinary_turn() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted — the front desk has it.", |seat| {
            panic!("a desk of one must never be handed a seat turn: {seat:?}")
        }),
        Duration::ZERO,
    )
    .await;
    let (address, runtime) = boot_lab(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    let body = client.say(FRONT, "Anything waiting at the front?").await;
    assert_eq!(
        body["responses"].as_array().map(Vec::len),
        Some(1),
        "{body}"
    );

    let rows = journal(&runtime).await;
    let desk = replies(&rows, FRONT);
    assert_eq!(desk.len(), 1, "one ordinary reply: {desk:?}");
    assert_eq!(desk[0].agent, GREETER);
    assert_eq!(desk[0].kind, None, "no episode metadata on a plain reply");
    assert!(
        !rows.iter().any(|row| matches!(
            &row.event,
            CompanyEvent::EpisodeOpened { .. }
                | CompanyEvent::RoundStarted { .. }
                | CompanyEvent::RoundCommitted { .. }
                | CompanyEvent::EpisodeCompleted { .. }
        )),
        "no episode frames: {:?}",
        rows.iter().map(|row| row.event.kind()).collect::<Vec<_>>()
    );
    assert!(
        script.asks().iter().all(|ask| seat_of(ask).is_none()),
        "no sentinel reached the model"
    );
    assert_eq!(report(&runtime).await.episodes_opened, 0);
}

// ---------------------------------------------------------------------------
// 5: a cross-desk referral crosses only the answer back
// ---------------------------------------------------------------------------

/// `@#<desk>` is the desk-mention spelling the mention resolver reads
/// (`tinyhivemind_core::mention`); a bare `#content` is prose.
const QUESTION: &str = "Please ask @#content for the release-note tagline.";
const TAGLINE: &str = "Checkout, now with fewer steps.";

/// Parked with the feature: cross-desk referral is not wired to the
/// conductor. The crossing lived in the hand-written driver
/// (`hive/driver/crossing.rs`) and went out with it; `hive::referral`
/// survives, but nothing in the episode path calls it, so no episode can
/// cross a question to another desk or bring an answer home.
///
/// Un-ignore when referral is reattached. Rewriting it to pass would be
/// worse than leaving it red.
#[ignore = "cross-desk referral is not reconnected to the conductor"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_desk_referral_crosses_only_the_answer_back() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted.", |seat| {
            match (seat.desk.as_str(), seat.speaker.as_str()) {
                // The desk mention rides the engineer's opening hand-off.
                (ENGINEERING, ENGINEER) if !seat.operator_asked().is_empty() => {
                    broadcast(seat, format!("Rollout plan drafted. {QUESTION}"))
                }
                // The far desk's answer *is* its recorded part.
                (CONTENT, WRITER) => complete(seat, format!("Tagline: {TAGLINE}")),
                _ => record_part(seat),
            }
        }),
        Duration::from_millis(50),
    )
    .await;
    let (address, runtime) = boot_lab(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    client
        .say(
            ENGINEERING,
            "Plan the rollout and get the release note written.",
        )
        .await;
    // Both desks' episodes complete, and the answer has come home — the
    // return marker is journaled after the answer row, so it is the marker
    // that says the crossing is over. `completed(2)` counts the
    // engineering desk's *first* completion (round 2, before the answer
    // ever arrives — its own CEO and engineer both call `complete_episode`
    // independently of the referral) together with the content desk's
    // completion, so it and the return marker can both be true while
    // `deliver_answer`'s reopened round for the engineer is still only
    // spawned (`crossing.rs`'s `spawn_drive`, a bare `tokio::spawn` the
    // driver does not await) and has not yet reached the model. Wait for
    // that reopened ask to actually land on the script too, or the
    // assertions below race it.
    let rows = wait_for(
        &runtime,
        "both episodes, the answer, and the reopened turn",
        EPISODE,
        |rows| {
            completed(2)(rows)
                && rows.iter().any(|row| {
                    matches!(
                        &row.event,
                        CompanyEvent::ReferralEnqueued {
                            returning: true,
                            ..
                        }
                    )
                })
                && script.asks().iter().filter_map(seat_of).any(|seat| {
                    seat.speaker == ENGINEER
                        && seat.prompt.contains("answered the question you put to it")
                })
        },
    )
    .await;

    // The forward marker, and the return: `(from_desk, to_desk, returning,
    // episode_id, to_episode_id)`.
    type Marker = (String, String, bool, Option<String>, Option<String>);
    let markers: Vec<Marker> = rows
        .iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::ReferralEnqueued {
                from_desk,
                to_desk,
                returning,
                episode_id,
                to_episode_id,
                ..
            } => Some((
                from_desk.clone(),
                to_desk.clone(),
                *returning,
                episode_id.clone(),
                to_episode_id.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(markers.len(), 2, "one crossing, one return: {markers:?}");
    assert_eq!(
        (markers[0].0.as_str(), markers[0].1.as_str(), markers[0].2),
        (ENGINEERING, CONTENT, false)
    );
    assert_eq!(
        (markers[1].0.as_str(), markers[1].1.as_str(), markers[1].2),
        (CONTENT, ENGINEERING, true)
    );
    let done = completions(&rows);
    let engineering_episode = done
        .iter()
        .find(|(_, chat, _, _)| chat == ENGINEERING)
        .map(|(id, ..)| id.clone())
        .expect("the engineering episode completed");
    let content_episode = done
        .iter()
        .find(|(_, chat, _, _)| chat == CONTENT)
        .map(|(id, ..)| id.clone())
        .expect("the content episode completed");
    // On both legs `episode_id` is the episode that ASKED and
    // `to_episode_id` the one that answered — the pair a console keys the
    // crossing on stays the same whichever way the frame is addressed.
    assert_eq!(markers[0].3.as_deref(), Some(engineering_episode.as_str()));
    assert_eq!(markers[1].3.as_deref(), Some(engineering_episode.as_str()));
    assert_eq!(markers[1].4.as_deref(), Some(content_episode.as_str()));
    let hops: Vec<(String, u32)> = rows
        .iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::EpisodeOpened { chat_id, hop, .. } => Some((chat_id.clone(), *hop)),
            _ => None,
        })
        .collect();
    assert_eq!(
        hops,
        vec![(ENGINEERING.to_string(), 0), (CONTENT.to_string(), 1)]
    );

    // Only the question crossed, under the referral author; only the answer
    // came home, under the same author. No seat spoke on the other desk.
    let content = replies(&rows, CONTENT);
    let seed = content.first().expect("the far desk starts with the seed");
    assert_eq!(seed.agent, HIVE_REFERRAL_AUTHOR);
    assert!(seed.text.contains(QUESTION), "{}", seed.text);
    assert!(
        content
            .iter()
            .all(|row| [WRITER, CEO, HIVE_REFERRAL_AUTHOR].contains(&row.agent.as_str())),
        "{content:?}"
    );
    let engineering = replies(&rows, ENGINEERING);
    assert!(
        engineering
            .iter()
            .all(|row| [ENGINEER, CEO, HIVE_REFERRAL_AUTHOR].contains(&row.agent.as_str())),
        "{engineering:?}"
    );
    let answer = engineering
        .iter()
        .find(|row| row.agent == HIVE_REFERRAL_AUTHOR)
        .unwrap();
    assert!(answer.text.contains(TAGLINE), "{}", answer.text);
    assert!(
        !answer.text.contains("Drafting the tagline"),
        "the far desk's working rows stay there: {}",
        answer.text
    );

    // The asker was reopened with the answer.
    let reopened = script
        .asks()
        .iter()
        .filter_map(seat_of)
        .find(|seat| {
            seat.speaker == ENGINEER && seat.prompt.contains("answered the question you put to it")
        })
        .expect("the engineer's reopened turn");
    // The assignment cites the answer row; the row itself reaches the seat
    // in the desk delta, attributed to the referral author.
    let delta = reopened
        .prompt
        .rsplit_once("## New desk messages\n")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    assert!(
        delta.contains(TAGLINE) && delta.contains(HIVE_REFERRAL_AUTHOR),
        "{}",
        reopened.prompt
    );

    let measured = report(&runtime).await;
    assert_eq!(measured.cross_desk_referrals, 1);
    assert_eq!(measured.referral_pairs, vec!["engineering→content"]);
    assert_eq!(measured.episodes_completed, 2);
    assert_eq!(measured.same_agent_overlaps, 0);
}

// ---------------------------------------------------------------------------
// 6: a shared agent on two desks
// ---------------------------------------------------------------------------

/// Whether two open brackets on different desks ever coincided.
fn cross_desk_overlap(rows: &[StoredEvent]) -> bool {
    let mut open: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for row in rows {
        match &row.event {
            CompanyEvent::TurnStarted {
                turn_id,
                chat_id,
                episode_id: Some(_),
                ..
            } => {
                if open.values().any(|desk| desk != chat_id) {
                    return true;
                }
                open.insert(turn_id.clone(), chat_id.clone());
            }
            CompanyEvent::TurnSettled { turn_id, .. }
            | CompanyEvent::TurnFailed { turn_id, .. } => {
                open.remove(turn_id);
            }
            _ => {}
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_agent_on_two_desks_runs_both_rooms_without_running_twice() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted.", record_part),
        Duration::from_millis(300),
    )
    .await;
    let (address, runtime) = boot_demo(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(DEMO_ADMIN).await;

    // Both desks at once; the CEO sits on both.
    let (first, second) = tokio::join!(
        client.say(ENGINEERING, "Plan the staging rollout."),
        client.say(CONTENT, "Draft the release note."),
    );
    assert!(first["responses"].is_array() && second["responses"].is_array());
    let rows = wait_for(&runtime, "both episodes to complete", EPISODE, completed(2)).await;

    let done = completions(&rows);
    assert_eq!(done.len(), 2, "{done:?}");
    assert!(
        done.iter()
            .all(|(_, _, reason, _)| *reason == EpisodeReason::CompleteEpisode),
        "{done:?}"
    );
    let desks: std::collections::BTreeSet<&str> =
        done.iter().map(|(_, chat, _, _)| chat.as_str()).collect();
    assert_eq!(desks, [ENGINEERING, CONTENT].into_iter().collect());

    // The brackets: overlap across desks, never for the CEO.
    assert!(
        cross_desk_overlap(&rows),
        "two desks' brackets never coincided: {:?}",
        rows.iter().map(|row| row.event.kind()).collect::<Vec<_>>()
    );
    let measured = report(&runtime).await;
    assert_eq!(
        measured.same_agent_overlaps, 0,
        "the CEO ran on two desks and never twice at once: {measured:?}"
    );
    assert!(measured.max_concurrent_turns >= 2, "{measured:?}");
    assert!(
        script.peak_in_flight() >= 2,
        "the model saw both desks at once: {}",
        script.peak_in_flight()
    );
    assert_eq!(measured.episodes_completed, 2);
    // Deliberately not a turn count. What this test is about is the lock:
    // the CEO sits on both desks, both desks ran at once, and it never ran
    // twice at once — asserted above by `same_agent_overlaps` and
    // `cross_desk_overlap`. How many turns each desk needed is the routing
    // plan's business, and a desk whose seat records straight away needs
    // one.
    let seat_turns = rows
        .iter()
        .filter(|row| {
            matches!(
                &row.event,
                CompanyEvent::TurnStarted {
                    agent_id: Some(_),
                    episode_id: Some(_),
                    ..
                }
            )
        })
        .count();
    assert!(seat_turns >= 2, "both desks ran a seat turn: {seat_turns}");

    let failures = measured.failures(&Thresholds {
        cross_desk_referrals: 0,
        agent_contacts: 0,
        distinct_pairs: 0,
        ..Thresholds::default()
    });
    assert!(failures.is_empty(), "{failures:?}");
}

// ---------------------------------------------------------------------------
// 7: (removed) a checkpoint replays the rows after it as a no-op
// ---------------------------------------------------------------------------
//
// The mechanism this covered is gone. The old loop checkpointed the driver's
// state and, on resume, replayed the journal rows written after it to catch
// the driver up. `run_episode` checkpoints the conductor's whole state --
// open conversations, watermarks, nudges, who is parked -- and resumes from
// that, so there are no rows to replay. `tinyhivemind`'s own suite covers
// snapshot and resume; there is nothing left here for this test to assert.

// ---------------------------------------------------------------------------
// 8: memory over the MCP memory tool
// ---------------------------------------------------------------------------

const FACT: &str = "The rollout window is Tuesday 09:00 UTC, agreed with support.";
const FACT_KEY: &str = "Tuesday 09:00 UTC";
const ASK_ONE: &str = "Fix the rollout window.";
const ASK_TWO: &str = "When is the rollout window?";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_desk_remembers_across_episodes_through_its_memory_tools() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted.", |seat| {
            if seat.speaker != ENGINEER || seat.operator_asked().is_empty() {
                return record_part(seat);
            }
            let memory = seat.last_tool_result();
            if seat.operator_asked() == ASK_ONE {
                return match memory {
                    // The memory tools are on the seat's own belt now, called
                    // by name. They reached the pooled agent over the
                    // `opencompany` MCP server because `openhuman_embed::Agent`
                    // has no seam for an in-process host tool; a session host
                    // does, and an episode seat is one.
                    None => Reply::Call {
                        tool: "memory_store",
                        args: json!({ "title": "rollout window", "body": FACT }),
                    },
                    Some(_) => complete(seat, "Window fixed and written down."),
                };
            }
            if seat.operator_asked() == ASK_TWO {
                return match memory {
                    None => Reply::Call {
                        tool: "memory_recall",
                        args: json!({ "query": "rollout window" }),
                    },
                    // Cite what memory handed back, never a constant.
                    Some(recalled) if recalled.contains(FACT_KEY) => {
                        complete(seat, format!("From the desk's memory: {FACT_KEY}."))
                    }
                    Some(recalled) => complete(seat, format!("Memory had nothing: {recalled}")),
                };
            }
            record_part(seat)
        }),
        Duration::from_millis(30),
    )
    .await;
    let (address, runtime) = boot_lab(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    client.say(ENGINEERING, ASK_ONE).await;
    let rows = wait_for(&runtime, "the first episode", EPISODE, completed(1)).await;
    assert!(
        replies(&rows, ENGINEERING)
            .iter()
            .any(|row| row.agent == ENGINEER && row.text.contains("written down")),
        "{:?}",
        replies(&rows, ENGINEERING)
    );
    let stored = script
        .asks()
        .iter()
        .filter_map(seat_of)
        .filter(|seat| seat.speaker == ENGINEER && seat.operator_asked() == ASK_ONE)
        .flat_map(|seat| seat.turn_tools)
        .collect::<Vec<_>>();
    assert!(
        stored.iter().any(|output| !is_refused(output)),
        "memory_store answered over MCP: {stored:?}"
    );

    client.say(ENGINEERING, ASK_TWO).await;
    let rows = wait_for(&runtime, "the second episode", EPISODE, completed(2)).await;
    dump(&rows, &script);
    let cited = replies(&rows, ENGINEERING)
        .into_iter()
        .rev()
        .find(|row| row.agent == ENGINEER && row.kind == Some(UtteranceKind::CompleteEpisode))
        .expect("the engineer's second-episode recorded part");
    assert!(
        cited.text.contains(FACT_KEY),
        "the recorded part cites what memory_recall returned: {}",
        cited.text
    );
    let recalled = script
        .asks()
        .iter()
        .filter_map(seat_of)
        .filter(|seat| seat.speaker == ENGINEER && seat.operator_asked() == ASK_TWO)
        .flat_map(|seat| seat.turn_tools)
        .collect::<Vec<_>>();
    assert!(
        recalled.iter().any(|output| output.contains(FACT_KEY)),
        "memory_recall came back with the fact: {recalled:?}"
    );
    every_seat_turn_is_grounded_and_styled(&script.asks());
    assert_eq!(report(&runtime).await.episodes_completed, 2);
}

// ---------------------------------------------------------------------------
// 8: a seat at two desks is shown the other one as context
// ---------------------------------------------------------------------------

/// **An agent seated at two desks knows the other exists.**
///
/// `hive_demo` seats the CEO at both `engineering` and `content`. Until the
/// host answered `Journal::channels`, a seat's brief was its own desk and
/// nothing else: the CEO ran both rooms and, on either turn, had no idea what
/// the other held. The library asks for those conversations so it can put
/// their newest rows in front of the seat as context -- "Nothing here is
/// addressed to you on this desk."
///
/// Two halves have to be right, and both are this host's: `channels` names
/// the desks, and `EventLogSessionLog::also_read` admits their rows. Naming a
/// desk the log refuses renders nothing, which is what this ran as before.
///
/// **Sequential on purpose.** The desks run one after the other, unlike
/// `a_shared_agent_on_two_desks_runs_both_rooms_without_running_twice` -- for
/// `elsewhere` to hold anything the *other* desk's rows must already be below
/// this wave's watermark, and two desks opened at once have nothing to show
/// each other yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_at_two_desks_is_shown_the_other_as_context() {
    let home = tempfile::tempdir().unwrap();
    // The hand-off script, because the fallback plan names one seat and it is
    // never the CEO: the seat it opens with broadcasts, the fallback places
    // that on the desk's other seat, and that is what brings the CEO in.
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted.", hand_off_then_record),
        Duration::from_millis(150),
    )
    .await;
    let (address, runtime) = boot_demo(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(DEMO_ADMIN).await;

    // The content desk first, and all the way to quiescence: its rows are
    // what the engineering brief should later carry as context.
    client.say(CONTENT, "Draft the release note.").await;
    wait_for(
        &runtime,
        "the content episode to complete",
        EPISODE,
        completed(1),
    )
    .await;

    client.say(ENGINEERING, "Plan the staging rollout.").await;
    let rows = wait_for(&runtime, "both episodes to complete", EPISODE, completed(2)).await;
    assert_eq!(completions(&rows).len(), 2);

    let seats: Vec<Seat> = script.asks().iter().filter_map(seat_of).collect();
    let elsewhere = |seat: &Seat| seat.prompt.contains("## Elsewhere, for context");

    // The CEO sits at both, so its engineering turn is shown the content desk.
    let ceo = seats
        .iter()
        .filter(|seat| seat.speaker == CEO && seat.desk == ENGINEERING)
        .find(|seat| elsewhere(seat))
        .unwrap_or_else(|| {
            panic!(
                "the CEO's engineering brief never carried the content desk; briefs: {:?}",
                seats
                    .iter()
                    .filter(|seat| seat.speaker == CEO)
                    .map(|seat| (&seat.desk, elsewhere(seat)))
                    .collect::<Vec<_>>()
            )
        });
    let section = ceo
        .prompt
        .rsplit_once("## Elsewhere, for context")
        .map(|(_, rest)| rest.to_string())
        .expect("the section is there");
    assert!(
        section.contains(CONTENT),
        "the section names the other desk: {section}"
    );
    assert!(
        section.contains(&format!("@{WRITER}")) || section.contains(&format!("@{CEO}")),
        "the section carries a row said on that desk: {section}"
    );

    // And a seat that sits at one desk only is shown nothing else -- this is
    // per seat, not per desk.
    assert!(
        seats
            .iter()
            .filter(|seat| seat.speaker == ENGINEER)
            .all(|seat| !elsewhere(seat)),
        "the engineer sits only at engineering and must be told it is in nothing else",
    );
}

// ---------------------------------------------------------------------------
// 9: a seat parks on the operator and the episode resumes on the decision
// ---------------------------------------------------------------------------

/// Boots the in-test company under `id`, so a second boot on the same home
/// is the same company after a restart.
async fn boot_named(home: &Path, id: &str, base_url: &str) -> (SocketAddr, Arc<CompanyRuntime>) {
    let manifest = CompanyManifest::from_stored_toml(&manifest(id, base_url))
        .expect("the in-test manifest parses");
    boot(home, id, manifest).await
}

/// Whether the seat's brief carries the operator's decision.
fn decided(seat: &Seat, words: &str) -> bool {
    seat.prompt.contains(words)
}

/// The engineer asks the operator first, and records its part once the
/// decision is in its brief. Every other seat records at once.
fn ask_then_record(
    ask: impl Fn(&Seat) -> Reply + Send + Sync + 'static,
    asked_with: &'static str,
    decision: &'static [&'static str],
) -> Responder {
    seat_script("Noted.", move |seat| {
        if seat.speaker != ENGINEER {
            return record_part(seat);
        }
        if let Some(words) = decision.iter().find(|words| decided(seat, words)) {
            return complete(seat, format!("Decision received: {words}"));
        }
        if seat.called(asked_with) {
            return Reply::Say(DONE.to_string());
        }
        ask(seat)
    })
}

fn request_approval(_: &Seat) -> Reply {
    Reply::Call {
        tool: "request_approval",
        args: json!({
            "title": "Email the client",
            "question": "May I email the client the rollout plan?"
        }),
    }
}

/// The pending approvals, as the console reads them.
async fn approvals(client: &Client) -> Vec<Value> {
    let (status, body) = client.get("/api/v1/company/approvals").await;
    assert_eq!(status, 200, "{body}");
    body.as_array().cloned().unwrap_or_default()
}

/// Polls until an approval raised by an episode seat is pending.
async fn seat_approval(client: &Client) -> Value {
    let started = Instant::now();
    loop {
        if let Some(found) = approvals(client)
            .await
            .into_iter()
            .find(|approval| approval.get("episode").is_some())
        {
            return found;
        }
        assert!(
            started.elapsed() < EPISODE,
            "no episode seat's approval appeared: {:?}",
            approvals(client).await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn seat_parked(rows: &[StoredEvent]) -> Vec<(String, String, Vec<String>)> {
    rows.iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::EpisodeSeatParked {
                episode_id,
                seat,
                approval_ids,
                ..
            } => Some((
                episode_id.clone(),
                seat.clone(),
                approval_ids
                    .iter()
                    .map(|id| id.as_ref().to_string())
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

fn seat_resumed(rows: &[StoredEvent]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::EpisodeSeatResumed { seat, .. } => Some(seat.clone()),
            _ => None,
        })
        .collect()
}

/// Parks the engineer on its first approval and checks the park is what the
/// console and the journal say it is. Returns the approval.
async fn parked_on_the_operator(client: &Client, runtime: &Arc<CompanyRuntime>) -> Value {
    client
        .say(
            ENGINEERING,
            "Plan the staging rollout for the new checkout.",
        )
        .await;
    let approval = seat_approval(client).await;
    assert_eq!(approval["episode"]["seat"], ENGINEER, "{approval}");
    assert_eq!(approval["thread"], ENGINEERING, "the card sits on the desk");
    let episode = approval["episode"]["id"].as_str().unwrap().to_string();
    let rows = wait_for(runtime, "the seat's park row", EPISODE, |rows| {
        !seat_parked(rows).is_empty()
    })
    .await;
    assert_eq!(
        seat_parked(&rows),
        vec![(
            episode.clone(),
            ENGINEER.to_string(),
            vec![approval["id"].as_str().unwrap().to_string()]
        )]
    );
    assert!(
        completions(&rows).is_empty(),
        "the episode waits on the operator rather than completing"
    );
    let (_, episodes) = client.get("/api/v1/company/episodes").await;
    assert_eq!(episodes[0]["waiting"], json!([ENGINEER]), "{episodes}");
    approval
}

async fn decide(client: &Client, approval: &Value, body: Value) {
    let id = approval["id"].as_str().unwrap();
    let (status, answer) = client
        .post(&format!("/api/v1/company/approvals/{id}"), body)
        .await;
    assert_eq!(status, 200, "the decision is refused: {answer}");
}

/// The engineer's turns that were shown `words`.
fn told(script: &support::script_model::Script, words: &str) -> usize {
    script
        .asks()
        .iter()
        .filter_map(seat_of)
        .filter(|seat| {
            seat.speaker == ENGINEER && seat.turn_tools.is_empty() && decided(seat, words)
        })
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approved_request_resumes_the_seat_and_completes_the_episode() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        ask_then_record(
            request_approval,
            "request_approval",
            &["approved your request", "denied your request"],
        ),
        Duration::from_millis(30),
    )
    .await;
    let (address, runtime) = boot_named(home.path(), &unique("hive-park"), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    let approval = parked_on_the_operator(&client, &runtime).await;
    decide(
        &client,
        &approval,
        json!({ "verdict": "approve", "detach": true }),
    )
    .await;

    let rows = wait_for(&runtime, "the episode to complete", EPISODE, completed(1)).await;
    dump(&rows, &script);
    assert_eq!(seat_resumed(&rows), vec![ENGINEER.to_string()]);
    assert_eq!(
        told(&script, "approved your request: Email the client"),
        1,
        "the resumed seat is shown the decision once"
    );
    assert!(
        replies(&rows, ENGINEERING)
            .iter()
            .any(|row| row.agent == ENGINEER && row.text.contains("approved your request")),
        "{:?}",
        replies(&rows, ENGINEERING)
    );
    assert!(approvals(&client).await.is_empty());
    let (_, episodes) = client.get("/api/v1/company/episodes").await;
    assert_eq!(episodes[0]["status"], "completed");
    assert!(episodes[0].get("waiting").is_none(), "{episodes}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_denied_request_resumes_the_seat_with_the_denial() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        ask_then_record(
            request_approval,
            "request_approval",
            &["approved your request", "denied your request"],
        ),
        Duration::from_millis(30),
    )
    .await;
    let (address, runtime) = boot_named(home.path(), &unique("hive-deny"), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    let approval = parked_on_the_operator(&client, &runtime).await;
    decide(
        &client,
        &approval,
        json!({ "verdict": "deny", "detach": true }),
    )
    .await;

    let rows = wait_for(&runtime, "the episode to complete", EPISODE, completed(1)).await;
    dump(&rows, &script);
    assert_eq!(told(&script, "denied your request: Email the client"), 1);
    assert_eq!(told(&script, "approved your request"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answered_escalation_reaches_the_seat_that_asked() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        ask_then_record(
            |_| Reply::Call {
                tool: "escalate_to_human",
                args: json!({ "question": "Which region do we roll out to first?" }),
            },
            "escalate_to_human",
            &["answered your question"],
        ),
        Duration::from_millis(30),
    )
    .await;
    let (address, runtime) = boot_named(home.path(), &unique("hive-ask"), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    let approval = parked_on_the_operator(&client, &runtime).await;
    decide(
        &client,
        &approval,
        json!({
            "verdict": "approve",
            "blocker_verdict": "amend",
            "blocker_answer": "eu-west first",
            "detach": true
        }),
    )
    .await;

    let rows = wait_for(&runtime, "the episode to complete", EPISODE, completed(1)).await;
    dump(&rows, &script);
    assert_eq!(told(&script, "\"eu-west first\""), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_episode_resumes_from_its_checkpoint_after_a_restart() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        ask_then_record(
            request_approval,
            "request_approval",
            &["approved your request"],
        ),
        Duration::from_millis(30),
    )
    .await;
    let id = unique("hive-restart");
    let approval = {
        let (address, runtime) = boot_named(home.path(), &id, &base_url).await;
        let client = Client::new(address);
        client.sign_in(ADMIN).await;
        parked_on_the_operator(&client, &runtime).await
    };

    let (address, runtime) = boot_named(home.path(), &id, &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;
    let pending = approvals(&client).await;
    assert_eq!(
        pending.iter().map(|a| a["id"].clone()).collect::<Vec<_>>(),
        vec![approval["id"].clone()],
        "the park survives the restart"
    );
    decide(
        &client,
        &approval,
        json!({ "verdict": "approve", "detach": true }),
    )
    .await;

    let rows = wait_for(
        &runtime,
        "the resumed episode to complete",
        EPISODE,
        completed(1),
    )
    .await;
    dump(&rows, &script);
    assert_eq!(
        completions(&rows)[0].0,
        approval["episode"]["id"].as_str().unwrap(),
        "the same episode completes, resumed rather than reopened"
    );
    assert_eq!(told(&script, "approved your request: Email the client"), 1);
}

// ---------------------------------------------------------------------------
// 10: a seat hands over a deliverable the operator can edit
// ---------------------------------------------------------------------------

const OUTLINE_PATH: &str = "pilot-outline.md";
const OUTLINE_TITLE: &str = "Pilot slide outline";
const OUTLINE: &str = "# Pilot slide outline\n\n1. Why now\n2. The pilot\n3. What it costs\n";

/// The engineer writes the outline, publishes it, and records its part.
fn write_publish_then_record(seat: &Seat) -> Reply {
    if seat.speaker != ENGINEER {
        return record_part(seat);
    }
    if !seat.called("file_write") {
        return Reply::Call {
            tool: "file_write",
            args: json!({ "path": OUTLINE_PATH, "content": OUTLINE }),
        };
    }
    if !seat.called("publish_artifact") {
        return Reply::Call {
            tool: "publish_artifact",
            args: json!({ "path": OUTLINE_PATH, "title": OUTLINE_TITLE }),
        };
    }
    complete(seat, "The pilot slide outline is published.")
}

/// Boots the in-test company with every tool on the belt, so a seat can
/// write a file and publish it.
async fn boot_tooled(home: &Path, base_url: &str) -> (SocketAddr, Arc<CompanyRuntime>) {
    let id = unique("hive-publish");
    let tooled = manifest(&id, base_url).replace("[tools]\nallow = []", "[tools]\nallow = [\"*\"]");
    assert!(
        tooled.contains("allow = [\"*\"]"),
        "the file tools are on the belt"
    );
    let manifest = CompanyManifest::from_stored_toml(&tooled).expect("the manifest parses");
    boot(home, &id, manifest).await
}

/// The engineer's replies on `chat`, as `(kind, text, outputs, task_id)`.
fn delivered_rows(
    rows: &[StoredEvent],
    chat: &str,
) -> Vec<(Option<UtteranceKind>, String, Value, Option<String>)> {
    rows.iter()
        .filter_map(|row| match &row.event {
            CompanyEvent::AgentReply {
                chat_id,
                agent_id,
                text,
                outputs,
                task_id,
                episode,
                ..
            } if chat_id == chat && agent_id == ENGINEER => Some((
                episode.as_ref().map(|episode| episode.kind),
                text.clone(),
                serde_json::to_value(outputs).unwrap(),
                task_id.clone(),
            )),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_publishes_a_deliverable_the_operator_can_edit() {
    let home = tempfile::tempdir().unwrap();
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted.", write_publish_then_record),
        Duration::from_millis(30),
    )
    .await;
    let (address, runtime) = boot_tooled(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    client
        .say(ENGINEERING, "Draft the slide outline for the pilot.")
        .await;
    let rows = wait_for(&runtime, "the episode to complete", EPISODE, completed(1)).await;
    dump(&rows, &script);

    let receipts: Vec<String> = script
        .asks()
        .iter()
        .filter_map(seat_of)
        .filter(|seat| seat.speaker == ENGINEER)
        .flat_map(|seat| {
            seat.calls
                .into_iter()
                .zip(seat.turn_tools)
                .filter(|(call, _)| call == "publish_artifact")
                .map(|(_, output)| output)
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        receipts
            .iter()
            .any(|output| output.contains("filed on a board card for this room")),
        "the seat is told where its file went, not refused: {receipts:?}"
    );

    let delivered = delivered_rows(&rows, ENGINEERING);
    let (kind, text, outputs, task_id) = delivered
        .iter()
        .find(|(kind, ..)| *kind == Some(UtteranceKind::CompleteEpisode))
        .cloned()
        .unwrap_or_else(|| panic!("the engineer recorded its part: {delivered:?}"));
    assert_eq!(kind, Some(UtteranceKind::CompleteEpisode));
    assert!(text.contains("published"), "{text}");
    let card = task_id.expect("the row links the card the file was filed on");
    let artifact = outputs
        .as_array()
        .and_then(|outputs| {
            outputs
                .iter()
                .find(|output| output["kind"] == "artifact")
                .cloned()
        })
        .unwrap_or_else(|| panic!("the row carries the artifact: {outputs}"));
    assert_eq!(artifact["taskId"], json!(card));
    assert_eq!(artifact["version"], json!(1));
    assert_eq!(
        delivered
            .iter()
            .filter(|(.., outputs, _)| outputs
                .as_array()
                .is_some_and(|outputs| !outputs.is_empty()))
            .count(),
        1,
        "handed over on one row: {delivered:?}"
    );

    let (status, history) = client
        .get(&format!("/api/v1/company/chat/history?desk={ENGINEERING}"))
        .await;
    assert_eq!(status, 200, "{history}");
    let shown = history
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["taskId"] == json!(card))
                .cloned()
        })
        .unwrap_or_else(|| panic!("the console reads the linked row: {history}"));
    assert_eq!(shown["outputs"][0]["targetId"], artifact["targetId"]);

    let (status, listed) = client
        .get(&format!("/api/v1/company/tasks/{card}/artifacts"))
        .await;
    assert_eq!(status, 200, "{listed}");
    let listed = listed.as_array().cloned().unwrap_or_default();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["id"], artifact["targetId"]);
    assert_eq!(listed[0]["title"], json!(OUTLINE_TITLE));
    assert_eq!(listed[0]["versions"][0]["body"], json!(OUTLINE));

    let edited = OUTLINE.replace("What it costs", "What it saves");
    let (status, revised) = client
        .post(
            &format!(
                "/api/v1/company/artifacts/{}/versions",
                artifact["targetId"].as_str().unwrap()
            ),
            json!({ "body": edited }),
        )
        .await;
    assert_eq!(status, 200, "{revised}");
    assert_eq!(revised["versions"][1]["author"], json!("operator"));
    assert_eq!(revised["humanEditDiff"]["fromVersion"], json!(1));
    assert_eq!(revised["humanEditDiff"]["toVersion"], json!(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_that_only_publishes_hands_over_on_a_row_of_its_own() {
    let home = tempfile::tempdir().unwrap();
    let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marked = Arc::clone(&published);
    let (base_url, script) = spawn_script_with_latency(
        seat_script("Noted.", move |seat| {
            if seat.speaker != ENGINEER {
                return record_part(seat);
            }
            if marked.load(std::sync::atomic::Ordering::SeqCst) {
                return complete(seat, "The outline is on its card.");
            }
            if !seat.called("file_write") {
                return Reply::Call {
                    tool: "file_write",
                    args: json!({ "path": OUTLINE_PATH, "content": OUTLINE }),
                };
            }
            if !seat.called("publish_artifact") {
                return Reply::Call {
                    tool: "publish_artifact",
                    args: json!({ "path": OUTLINE_PATH, "title": OUTLINE_TITLE }),
                };
            }
            marked.store(true, std::sync::atomic::Ordering::SeqCst);
            Reply::Say(DONE.to_string())
        }),
        Duration::from_millis(30),
    )
    .await;
    let (address, runtime) = boot_tooled(home.path(), &base_url).await;
    let client = Client::new(address);
    client.sign_in(ADMIN).await;

    client
        .say(ENGINEERING, "Draft the slide outline for the pilot.")
        .await;
    let rows = wait_for(&runtime, "the episode to complete", EPISODE, completed(1)).await;
    dump(&rows, &script);
    assert!(published.load(std::sync::atomic::Ordering::SeqCst));

    let delivered = delivered_rows(&rows, ENGINEERING);
    let handed: Vec<_> = delivered
        .iter()
        .filter(|(.., outputs, _)| {
            outputs
                .as_array()
                .is_some_and(|outputs| !outputs.is_empty())
        })
        .collect();
    assert_eq!(handed.len(), 1, "handed over once: {delivered:?}");
    let (kind, text, outputs, task_id) = handed[0];
    assert!(
        text.is_empty(),
        "a row of its own, with nothing said: {text}"
    );
    assert_eq!(
        *kind,
        Some(UtteranceKind::Post),
        "it belongs to the episode"
    );
    assert_eq!(outputs[0]["kind"], json!("artifact"));
    assert_eq!(outputs[0]["taskId"], json!(task_id.clone().unwrap()));
    let recorded = delivered
        .iter()
        .position(|(kind, ..)| *kind == Some(UtteranceKind::CompleteEpisode))
        .expect("the engineer recorded its part on a later turn");
    let handed_at = delivered
        .iter()
        .position(|(_, text, ..)| text.is_empty())
        .unwrap();
    assert!(
        handed_at < recorded,
        "handed over when its turn's wave ended"
    );
}
