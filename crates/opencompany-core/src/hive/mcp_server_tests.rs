//! The MCP server driven by the client that will call it in production —
//! `tinymcp::McpHttpClient`, OpenHuman's own — over a real loopback socket.

use super::*;
use crate::company::Policy;
use crate::harness::policy::{ApprovalRequestQueue, ApprovalScope};
use crate::hive::tools::{HiveTurn, InFlight, InFlightContext};
use crate::ports::events::EventStreamItem;
use crate::ports::types::{CompanyEvent, EventSeq, StoredEvent};
use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use serde_json::json;
use tinyhivemind_embed::{ConversationKind, ConversationRef};
use tinymcp::McpHttpClient;
use tinymcp::tinymcp_bus::McpAuthConfig as ClientAuth;
use tinytools::{ToolCallOptions, ToolResult, ToolRunContext};

const COMPANY: &str = "acme";
const AGENT: &str = "ceo";
const RUNTIME_ID: &str = "acme--ceo";

/// A journal that replays a fixed history.
struct FixedLog(Vec<StoredEvent>);

#[async_trait]
impl EventLog for FixedLog {
    async fn append(&self, _id: &CompanyId, _e: CompanyEvent) -> crate::Result<EventSeq> {
        unreachable!("read only")
    }
    async fn read_from(
        &self,
        _id: &CompanyId,
        seq: EventSeq,
        limit: usize,
    ) -> crate::Result<Vec<StoredEvent>> {
        Ok(self
            .0
            .iter()
            .filter(|e| e.seq >= seq)
            .take(limit)
            .cloned()
            .collect())
    }
    fn subscribe(&self, _id: &CompanyId) -> BoxStream<'static, EventStreamItem> {
        Box::pin(stream::empty())
    }
}

fn stored(seq: u64, event: CompanyEvent) -> StoredEvent {
    StoredEvent {
        seq: EventSeq::new(seq),
        company: CompanyId::new(COMPANY),
        event,
        at_millis: seq * 1000,
    }
}

fn operator_says(seq: u64, chat: &str, text: &str) -> StoredEvent {
    stored(
        seq,
        CompanyEvent::OperatorMessage {
            text: text.to_string(),
            by: None,
            chat: Some(chat.to_string()),
            parent: None,
            deliverable: None,
            mentions: Vec::new(),
            attachments: Vec::new(),
        },
    )
}

fn agent_says(seq: u64, chat: &str, agent: &str, text: &str, audience: &[&str]) -> StoredEvent {
    stored(
        seq,
        CompanyEvent::AgentReply {
            chat_id: chat.to_string(),
            agent_id: agent.to_string(),
            text: text.to_string(),
            steps: Vec::new(),
            outputs: Vec::new(),
            task_id: None,
            parent: None,
            mentions: Vec::new(),
            mention_depth: 0,
            audience: audience.iter().map(|a| (*a).to_string()).collect(),
            episode: None,
        },
    )
}

/// A served tool that reports which turn it ran under.
struct WhoAmI;

#[async_trait]
impl Tool for WhoAmI {
    fn name(&self) -> &str {
        "who_am_i"
    }
    fn description(&self) -> &str {
        "names the in-flight turn"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "greeting": { "type": "string" } } })
    }
    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::error("no context"))
    }
    async fn execute_with_context(
        &self,
        args: Value,
        _options: ToolCallOptions,
        context: Option<&dyn ToolRunContext>,
    ) -> anyhow::Result<ToolResult> {
        let turn = context
            .and_then(ToolRunContext::host_extension)
            .and_then(|ext| ext.downcast_ref::<InFlightContext>())
            .and_then(|ctx| ctx.turn.clone());
        let Some(turn) = turn else {
            return Ok(ToolResult::success("no turn in flight"));
        };
        let hive = turn.hive.as_ref();
        Ok(ToolResult::success(format!(
            "{}: agent={} surface={} episode={} revision={}",
            args["greeting"].as_str().unwrap_or("hello"),
            turn.agent_id,
            turn.surface.id,
            hive.map_or("-", |h| h.episode_id.as_str()),
            hive.map_or(u64::MAX, |h| h.revision),
        )))
    }
}

fn desk_turn() -> InFlight {
    InFlight::new(
        CompanyId::new(COMPANY),
        RUNTIME_ID,
        AGENT,
        ConversationRef {
            id: "engineering".to_string(),
            kind: ConversationKind::Desk,
            thread_root: None,
        },
    )
    .with_hive(HiveTurn {
        desk_id: "engineering".to_string(),
        episode_id: "ep-7".to_string(),
        revision: 2,
        turn_id: "turn-9".to_string(),
        members: vec![AGENT.to_string(), "engineer".to_string()],
    })
}

fn journal() -> Arc<dyn EventLog> {
    Arc::new(FixedLog(vec![
        operator_says(1, "engineering", "ship it"),
        agent_says(2, "engineering", "engineer", "on it", &[]),
        agent_says(
            3,
            "engineering",
            "engineer",
            "private to writer",
            &["writer"],
        ),
        agent_says(4, "content", "writer", "elsewhere", &[]),
        agent_says(5, "engineering", "engineer", "private to ceo", &[AGENT]),
    ]))
}

/// Serves one registered agent on a loopback port and returns the host, the
/// agent and a client authenticated as it.
async fn boot(agent: McpAgent) -> (Arc<McpHost>, Arc<McpAgent>, McpHttpClient) {
    let host = McpHost::new();
    let agent = host.register(agent);
    let addr = host.serve_loopback().await.expect("loopback bind");
    assert_eq!(host.serve_loopback().await.unwrap(), addr, "idempotent");
    let endpoint = host
        .endpoint_for(&agent.company, &agent.runtime_agent_id)
        .unwrap();
    assert_eq!(
        endpoint,
        format!("http://{addr}{MCP_PATH_PREFIX}/{COMPANY}/{RUNTIME_ID}")
    );
    let client = McpHttpClient::builder(endpoint)
        .timeout_secs(10)
        .auth(ClientAuth::BearerToken {
            token: agent.bearer().to_string(),
        })
        .build()
        .unwrap();
    (host, agent, client)
}

fn plain_agent() -> McpAgent {
    McpAgent::new(
        CompanyId::new(COMPANY),
        AGENT,
        RUNTIME_ID,
        McpAgent::mint_bearer(),
    )
    .tools(vec![Arc::new(WhoAmI) as Arc<dyn Tool>])
    .events(journal())
}

#[tokio::test]
async fn initialize_and_list_tools_serve_speech_and_custom_tools() {
    let (_host, agent, client) = boot(plain_agent()).await;
    let init = client.initialize().await.expect("initialize");
    assert_eq!(init.server_info["name"], SERVER_SLUG);
    let tools = client.list_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "post",
            "broadcast",
            "dm",
            // `ask` opens a conversation with one seat; it arrived with the
            // conductor and is served because the vocabulary is derived from
            // the library rather than mirrored here.
            "ask",
            "complete_episode",
            "read",
            "who_am_i"
        ]
    );
    assert_eq!(agent.allow_tools(), names);
    let dm = tools.iter().find(|t| t.name == "dm").unwrap();
    assert_eq!(dm.input_schema["properties"]["to"]["type"], "array");
}

#[tokio::test]
async fn an_unknown_bearer_is_401() {
    let (host, agent, _client) = boot(plain_agent()).await;
    let endpoint = host
        .endpoint_for(&agent.company, &agent.runtime_agent_id)
        .unwrap();
    let wrong = McpHttpClient::builder(endpoint.clone())
        .timeout_secs(10)
        .auth(ClientAuth::BearerToken {
            token: "ocm_not_the_one".to_string(),
        })
        .build()
        .unwrap();
    let error = wrong.initialize().await.expect_err("wrong bearer");
    assert!(
        matches!(error, tinymcp::Error::Unauthorized { .. }),
        "{error:?}"
    );

    let none = McpHttpClient::new(endpoint, 10).unwrap();
    let error = none.initialize().await.expect_err("no bearer");
    assert!(
        matches!(error, tinymcp::Error::Unauthorized { .. }),
        "{error:?}"
    );

    // The right bearer on the wrong route is refused too: a token names one
    // agent, and the path has to agree with it.
    let other_route = format!(
        "http://{}{MCP_PATH_PREFIX}/{COMPANY}/acme--engineer",
        host.addr().unwrap()
    );
    let misrouted = McpHttpClient::builder(other_route)
        .timeout_secs(10)
        .auth(ClientAuth::BearerToken {
            token: agent.bearer().to_string(),
        })
        .build()
        .unwrap();
    assert!(matches!(
        misrouted.initialize().await.expect_err("misrouted"),
        tinymcp::Error::Unauthorized { .. }
    ));
}

#[tokio::test]
async fn a_post_lands_in_the_outbox_and_a_second_action_errors() {
    let (host, _agent, client) = boot(plain_agent()).await;
    let ticket = host.in_flight().begin(desk_turn()).unwrap();

    let first = client
        .call_tool("post", json!({ "message": "B holds at 10^18" }))
        .await
        .expect("post");
    assert!(!first.rendered.is_error, "{}", first.rendered.output());
    assert!(first.rendered.output().starts_with("recorded: post"));

    let second = client
        .call_tool("complete_episode", json!({ "message": "done" }))
        .await
        .expect("the call succeeds; the tool says no");
    assert!(second.rendered.is_error);
    assert!(
        second
            .rendered
            .output()
            .contains("one action per turn; your first action is recorded"),
        "{}",
        second.rendered.output()
    );

    let turn = ticket.finish();
    assert_eq!(
        turn.outbox,
        vec![tinyhivemind::speech::Utterance::Post {
            message: "B holds at 10^18".to_string()
        }]
    );
}

#[tokio::test]
async fn speech_without_a_turn_in_flight_is_a_tool_error() {
    let (_host, _agent, client) = boot(plain_agent()).await;
    let result = client
        .call_tool("post", json!({ "message": "into the void" }))
        .await
        .unwrap();
    assert!(result.rendered.is_error);
    assert!(result.rendered.output().contains("no turn is in flight"));
}

#[tokio::test]
async fn a_dm_to_a_non_member_is_a_tool_error_containing_refused() {
    let (host, _agent, client) = boot(plain_agent()).await;
    let ticket = host.in_flight().begin(desk_turn()).unwrap();
    let refused = client
        .call_tool("dm", json!({ "to": ["writer"], "message": "psst" }))
        .await
        .unwrap();
    assert!(refused.rendered.is_error);
    assert!(
        refused.rendered.output().contains("refused"),
        "{}",
        refused.rendered.output()
    );
    assert!(ticket.snapshot().outbox.is_empty());

    let ok = client
        .call_tool("dm", json!({ "to": ["engineer"], "message": "quietly" }))
        .await
        .unwrap();
    assert!(!ok.rendered.is_error, "{}", ok.rendered.output());
    assert_eq!(ticket.finish().outbox.len(), 1);
}

/// A Phase 4 stand-in that refuses everyone.
struct NobodyResolver;

impl DmResolver for NobodyResolver {
    fn resolve_dm(
        &self,
        _company: &CompanyId,
        desk_id: &str,
        speaker: &str,
        to: &[String],
    ) -> Result<(), String> {
        Err(format!(
            "{speaker} may not dm {} on {desk_id}",
            to.join(",")
        ))
    }
}

#[tokio::test]
async fn an_installed_dm_resolver_outranks_the_membership_snapshot() {
    let (host, _agent, client) = boot(plain_agent()).await;
    host.set_dm_resolver(Arc::new(NobodyResolver));
    let ticket = host.in_flight().begin(desk_turn()).unwrap();
    let refused = client
        .call_tool("dm", json!({ "to": ["engineer"], "message": "quietly" }))
        .await
        .unwrap();
    assert!(refused.rendered.is_error);
    assert!(
        refused
            .rendered
            .output()
            .contains("refused: ceo may not dm engineer on engineering"),
        "{}",
        refused.rendered.output()
    );
    assert!(
        ticket.finish().outbox.is_empty(),
        "a refused dm is unrecorded"
    );
}

#[tokio::test]
async fn read_serves_the_conversation_narrowed_to_what_the_agent_may_see() {
    let (host, _agent, client) = boot(plain_agent()).await;
    let _ticket = host.in_flight().begin(desk_turn()).unwrap();
    let page = client.call_tool("read", json!({})).await.unwrap();
    assert!(!page.rendered.is_error, "{}", page.rendered.output());
    let text = page.rendered.output();
    assert_eq!(
        text,
        "[1] operator: ship it\n[2] engineer: on it\n[5] engineer: private to ceo"
    );
    let limited = client
        .call_tool("read", json!({ "limit": 1 }))
        .await
        .unwrap();
    assert!(
        limited
            .rendered
            .output()
            .starts_with("[5] engineer: private to ceo")
    );
    assert!(
        limited
            .rendered
            .output()
            .contains("Older messages are not in this reply")
    );
}

#[tokio::test]
async fn the_belt_read_answers_what_the_served_read_answers() {
    let (host, _agent, client) = boot(plain_agent()).await;
    let bound = Arc::new(std::sync::OnceLock::new());
    let read = crate::hive::tools::ConversationReadTool::new(
        Arc::clone(host.in_flight()),
        Arc::clone(&bound),
        journal(),
    );
    assert_eq!(read.name(), crate::hive::tools::READ_TOOL);
    assert_eq!(
        read.parameters_schema()["properties"]["limit"]["maximum"],
        100
    );

    let unbound = read.execute(json!({})).await.unwrap();
    assert!(unbound.is_error, "an unbound read has no turn to read");
    bound.set(RUNTIME_ID.to_string()).unwrap();
    let idle = read.execute(json!({})).await.unwrap();
    assert!(idle.is_error);
    assert!(
        idle.output().contains("no turn is in flight"),
        "{}",
        idle.output()
    );

    let _ticket = host.in_flight().begin(desk_turn()).unwrap();
    for args in [json!({}), json!({ "limit": 1 })] {
        let native = read.execute(args.clone()).await.unwrap();
        let served = client.call_tool("read", args).await.unwrap();
        assert!(!native.is_error, "{}", native.output());
        assert_eq!(native.output(), served.rendered.output());
    }
}

#[tokio::test]
async fn a_custom_tool_runs_with_the_in_flight_turn_as_its_context() {
    let (host, _agent, client) = boot(plain_agent()).await;
    let ticket = host.in_flight().begin(desk_turn()).unwrap();
    let result = client
        .call_tool("who_am_i", json!({ "greeting": "hi" }))
        .await
        .unwrap();
    assert!(!result.rendered.is_error);
    assert_eq!(
        result.rendered.output(),
        "hi: agent=ceo surface=engineering episode=ep-7 revision=2"
    );
    drop(ticket);
    let result = client.call_tool("who_am_i", json!({})).await.unwrap();
    assert_eq!(result.rendered.output(), "no turn in flight");

    let unknown = client.call_tool("no_such_tool", json!({})).await.unwrap();
    assert!(unknown.rendered.is_error);
    assert!(unknown.rendered.output().contains("unknown tool"));
}

#[tokio::test]
async fn the_policy_parks_or_denies_a_custom_tool_call() {
    let queue = ApprovalRequestQueue::default();
    let policy = ApprovalPolicy::new(
        &Policy {
            mode: "full".to_string(),
            always_approve: vec!["who_am_i".to_string()],
            auto_approve_under_usd: None,
            approval_ttl_hours: None,
        },
        None,
    )
    .with_requests(queue.clone())
    .with_agent(AGENT);
    let (host, agent, client) = boot(plain_agent().policy(Arc::new(policy))).await;
    let _ticket = host.in_flight().begin(desk_turn()).unwrap();

    let unrecorded = client.call_tool("who_am_i", json!({})).await.unwrap();
    assert!(unrecorded.rendered.is_error);
    assert!(
        unrecorded.rendered.output().contains("nobody was asked"),
        "a call no claim can park is refused, not reported as parked: {}",
        unrecorded.rendered.output()
    );

    let cycle = queue.claim(ApprovalScope::Cycle);
    let parked = cycle
        .scoped(agent.serve_call("who_am_i", json!({}), None))
        .await;
    assert_eq!(parked["isError"], json!(true), "{parked}");
    assert!(parked.to_string().contains("awaiting approval"), "{parked}");
    let drained = cycle.drain(8);
    assert_eq!(
        drained.requests.len(),
        1,
        "the park reached the claim's queue"
    );
    assert_eq!(drained.requests[0].tool, "who_am_i");

    let readonly = ApprovalPolicy::new(
        &Policy {
            mode: "readonly".to_string(),
            always_approve: Vec::new(),
            auto_approve_under_usd: None,
            approval_ttl_hours: None,
        },
        None,
    );
    let denied_agent = McpAgent::new(
        CompanyId::new(COMPANY),
        AGENT,
        "acme--ceo-ro",
        McpAgent::mint_bearer(),
    )
    .tools(vec![Arc::new(WhoAmI) as Arc<dyn Tool>])
    .policy(Arc::new(readonly));
    let endpoint = format!(
        "http://{}{MCP_PATH_PREFIX}/{COMPANY}/acme--ceo-ro",
        host.addr().unwrap()
    );
    let bearer = denied_agent.bearer().to_string();
    host.register(denied_agent);
    let ro = McpHttpClient::builder(endpoint)
        .timeout_secs(10)
        .auth(ClientAuth::BearerToken { token: bearer })
        .build()
        .unwrap();
    // `readonly` fails closed on a tool it cannot classify: a deny, which
    // reaches the seat as a tool error that says so. The finer deny arms are
    // `policy`'s own suite's; what this pins is the wire shape.
    let denied = ro.call_tool("who_am_i", json!({})).await.unwrap();
    assert!(denied.rendered.is_error);
    assert!(
        denied.rendered.output().starts_with("refused: 'who_am_i'"),
        "{}",
        denied.rendered.output()
    );
}

#[tokio::test]
async fn unregistering_an_agent_revokes_its_bearer() {
    let (host, agent, client) = boot(plain_agent()).await;
    client.initialize().await.unwrap();
    host.unregister_company(&agent.company);
    assert!(host.agent(RUNTIME_ID).is_none());
    let error = client.list_tools().await.expect_err("revoked");
    assert!(
        matches!(error, tinymcp::Error::Unauthorized { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn mount_serves_the_same_route_on_a_caller_router() {
    let host = McpHost::new();
    let agent = host.register(plain_agent());
    let app = mount(Router::new(), host.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = McpHttpClient::builder(format!(
        "http://{addr}{MCP_PATH_PREFIX}/{COMPANY}/{RUNTIME_ID}"
    ))
    .timeout_secs(10)
    .auth(ClientAuth::BearerToken {
        token: agent.bearer().to_string(),
    })
    .build()
    .unwrap();
    assert_eq!(client.list_tools().await.unwrap().len(), 7);
}

#[test]
fn attach_opencompany_mcp_names_the_server_slug_and_allow_list() {
    let host = McpHost::new();
    let agent = host.register(plain_agent());
    assert!(
        McpAttach::for_agent(&host, &agent).is_none(),
        "no endpoint before the listener is up"
    );
    let ctx = McpAttach {
        endpoint: format!("http://127.0.0.1:1{MCP_PATH_PREFIX}/{COMPANY}/{RUNTIME_ID}"),
        bearer: agent.bearer().to_string(),
        allow_tools: agent.allow_tools(),
    };
    let server = opencompany_mcp_server(&ctx);
    assert_eq!(server.name(), SERVER_SLUG);
    let rendered = format!("{server:?}");
    assert!(rendered.contains(&ctx.endpoint), "{rendered}");
    assert!(rendered.contains("who_am_i"), "{rendered}");
    assert!(rendered.contains("\"post\""), "{rendered}");
    assert!(
        !rendered.contains(agent.bearer()),
        "the bearer is redacted from the server's Debug"
    );
    // The spec accepts it; nothing on `AgentSpec` reads it back, so the
    // registration is proven by the runtime, not here.
    let spec = attach_opencompany_mcp(AgentSpec::new(RUNTIME_ID), &ctx);
    assert_eq!(spec.id(), RUNTIME_ID);
}
