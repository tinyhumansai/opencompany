//! Unit tests for the in-flight registry, the speech fold and the adapter.

use super::*;
use async_trait::async_trait;
use tinyhivemind_embed::ConversationKind;

fn desk_surface(id: &str) -> ConversationRef {
    ConversationRef {
        id: id.to_string(),
        kind: ConversationKind::Desk,
        thread_root: None,
    }
}

fn hive_turn(members: &[&str]) -> HiveTurn {
    HiveTurn {
        desk_id: "engineering".to_string(),
        episode_id: "ep-1".to_string(),
        revision: 0,
        turn_id: "turn-1".to_string(),
        members: members.iter().map(|m| (*m).to_string()).collect(),
    }
}

fn desk_turn(agent: &str, members: &[&str]) -> InFlight {
    InFlight::new(
        CompanyId::new("acme"),
        format!("acme--{agent}"),
        agent,
        desk_surface("engineering"),
    )
    .with_hive(hive_turn(members))
}

#[test]
fn a_post_lands_on_the_outbox_and_is_receipted() {
    let mut turn = desk_turn("ceo", &["ceo", "engineer"]);
    let reply = turn.speak("post", &json!({ "message": "  B holds at 10^18 " }));
    let Speech::Recorded(receipt) = reply else {
        panic!("expected a recorded post, got {reply:?}");
    };
    assert!(receipt.starts_with("recorded: post"), "{receipt}");
    assert_eq!(
        turn.outbox,
        vec![Utterance::Post {
            message: "B holds at 10^18".to_string()
        }]
    );
}

#[test]
fn a_second_action_in_one_turn_is_refused_and_the_first_stands() {
    let mut turn = desk_turn("ceo", &["ceo", "engineer"]);
    assert!(matches!(
        turn.speak("post", &json!({ "message": "first" })),
        Speech::Recorded(_)
    ));
    let second = turn.speak("complete_episode", &json!({ "message": "second" }));
    let Speech::Refused(text) = second else {
        panic!("expected a refusal, got {second:?}");
    };
    assert!(text.contains("refused"), "{text}");
    assert!(
        text.contains("one action per turn; your first action is recorded"),
        "{text}"
    );
    assert_eq!(turn.outbox.len(), 1, "the first utterance stands alone");
}

#[test]
fn read_is_never_an_action() {
    let mut turn = desk_turn("ceo", &["ceo", "engineer"]);
    assert_eq!(
        turn.speak("read", &json!({ "limit": 7 })),
        Speech::Read { limit: 7 }
    );
    assert_eq!(
        turn.speak("read", &json!({ "limit": 100_000 })),
        Speech::Read {
            limit: speech::READ_MAX
        }
    );
    assert!(matches!(
        turn.speak("post", &json!({ "message": "after reading" })),
        Speech::Recorded(_)
    ));
    assert_eq!(
        turn.speak("read", &json!({})),
        Speech::Read {
            limit: speech::READ_DEFAULT
        },
        "a read after the action is still served"
    );
}

#[test]
fn a_dm_to_a_desk_member_is_recorded() {
    let mut turn = desk_turn("ceo", &["ceo", "engineer"]);
    let reply = turn.speak("dm", &json!({ "to": ["@engineer"], "message": "quietly" }));
    assert!(matches!(reply, Speech::Recorded(_)), "{reply:?}");
    assert_eq!(
        turn.outbox,
        vec![Utterance::Dm {
            to: vec!["engineer".to_string()],
            message: "quietly".to_string()
        }]
    );
}

#[test]
fn a_dm_to_a_non_member_is_refused_with_the_word_refused() {
    let mut turn = desk_turn("ceo", &["ceo", "engineer"]);
    let reply = turn.speak("dm", &json!({ "to": ["writer"], "message": "psst" }));
    let Speech::Refused(text) = reply else {
        panic!("expected a refusal, got {reply:?}");
    };
    assert!(text.contains("refused"), "{text}");
    assert!(text.contains("writer"), "{text}");
    assert!(turn.outbox.is_empty(), "a refused dm records nothing");
}

#[test]
fn a_dm_only_to_oneself_or_outside_an_episode_is_refused() {
    let mut turn = desk_turn("ceo", &["ceo", "engineer"]);
    let reply = turn.speak("dm", &json!({ "to": ["ceo"], "message": "me" }));
    assert!(
        matches!(&reply, Speech::Refused(t) if t.contains("refused")),
        "{reply:?}"
    );

    let mut direct = InFlight::new(
        CompanyId::new("acme"),
        "acme--ceo",
        "ceo",
        ConversationRef {
            id: "dm:ceo".to_string(),
            kind: ConversationKind::Direct,
            thread_root: None,
        },
    );
    let reply = direct.speak("dm", &json!({ "to": ["engineer"], "message": "hi" }));
    assert!(
        matches!(&reply, Speech::Refused(t) if t.contains("refused") && t.contains("episode")),
        "{reply:?}"
    );
}

#[test]
fn malformed_calls_are_refused_in_the_seats_own_words() {
    let mut turn = desk_turn("ceo", &["ceo"]);
    let reply = turn.speak("post", &json!({}));
    assert!(
        matches!(&reply, Speech::Refused(t) if t == "refused: `message` must be a non-empty string"),
        "{reply:?}"
    );
    let reply = turn.speak("dm", &json!({ "message": "nobody" }));
    assert!(
        matches!(&reply, Speech::Refused(t) if t.contains("`to` must name at least one seat")),
        "{reply:?}"
    );
    let reply = turn.speak("shout", &json!({ "message": "x" }));
    assert!(
        matches!(&reply, Speech::Refused(t) if t.contains("unknown tool")),
        "{reply:?}"
    );
}

#[test]
fn the_registry_holds_one_turn_per_agent() {
    let registry = Arc::new(InFlightRegistry::new());
    let ticket = registry
        .begin(desk_turn("ceo", &["ceo"]))
        .expect("first turn registers");
    assert!(registry.is_in_flight("acme--ceo"));
    let second = registry.begin(desk_turn("ceo", &["ceo"]));
    assert_eq!(
        second.err(),
        Some(InFlightError::AlreadyInFlight {
            runtime_agent_id: "acme--ceo".to_string()
        })
    );
    let other = registry
        .begin(desk_turn("engineer", &["engineer"]))
        .expect("a different agent runs concurrently");
    assert_eq!(registry.in_flight(), vec!["acme--ceo", "acme--engineer"]);

    registry
        .with("acme--ceo", |turn| {
            turn.speak("post", &json!({ "message": "hi" }))
        })
        .expect("the turn is registered");
    let finished = ticket.finish();
    assert_eq!(finished.outbox.len(), 1);
    assert!(!registry.is_in_flight("acme--ceo"));
    assert!(registry.is_in_flight("acme--engineer"));
    drop(other);
    assert!(
        registry.in_flight().is_empty(),
        "dropping a ticket deregisters"
    );
}

#[test]
fn finishing_a_turn_frees_the_agent_for_the_next_one() {
    let registry = Arc::new(InFlightRegistry::new());
    let first = registry.begin(desk_turn("ceo", &["ceo"])).unwrap();
    let finished = first.finish();
    assert!(finished.outbox.is_empty());
    let second = registry
        .begin(desk_turn("ceo", &["ceo"]))
        .expect("the agent is free again");
    assert!(registry.is_in_flight("acme--ceo"));
    assert_eq!(second.runtime_agent_id(), "acme--ceo");
    assert_eq!(second.snapshot().agent_id, "ceo");
    drop(second);
    assert!(!registry.is_in_flight("acme--ceo"));
}

#[test]
fn speech_specs_render_to_mcp_descriptors_with_the_contract_argument_names() {
    let descriptors: Vec<Value> = speech::tool_specs().iter().map(speech_descriptor).collect();
    let names: Vec<&str> = descriptors
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        speech_tool_names(),
        "the descriptors and the names come from the same specs"
    );
    assert!(
        names.contains(&"ask"),
        "the vocabulary the library defines includes `ask`: {names:?}"
    );
    let dm = descriptors.iter().find(|d| d["name"] == "dm").unwrap();
    assert_eq!(dm["inputSchema"]["properties"]["to"]["type"], "array");
    assert_eq!(
        dm["inputSchema"]["properties"]["to"]["items"]["type"],
        "string"
    );
    assert_eq!(dm["inputSchema"]["properties"]["message"]["type"], "string");
    assert_eq!(dm["inputSchema"]["required"], json!(["to", "message"]));
    let read = descriptors.iter().find(|d| d["name"] == "read").unwrap();
    assert_eq!(
        read["inputSchema"]["properties"]["limit"]["type"],
        "integer"
    );
    assert_eq!(read["inputSchema"]["required"], json!([]));
    for descriptor in &descriptors {
        assert!(
            descriptor["description"]
                .as_str()
                .is_some_and(|d| !d.is_empty()),
            "every spec description is rendered verbatim"
        );
    }
}

/// A tool that reports the context it ran under.
struct ContextEcho;

#[async_trait]
impl Tool for ContextEcho {
    fn name(&self) -> &str {
        "context_echo"
    }
    fn description(&self) -> &str {
        "echoes the run context"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::error("executed without a context"))
    }
    async fn execute_with_context(
        &self,
        _args: Value,
        _options: ToolCallOptions,
        context: Option<&dyn ToolRunContext>,
    ) -> anyhow::Result<ToolResult> {
        let Some(context) = context else {
            return self.execute(Value::Null).await;
        };
        let turn = context
            .host_extension()
            .and_then(|ext| ext.downcast_ref::<InFlightContext>())
            .and_then(|ctx| ctx.turn.as_ref());
        Ok(ToolResult::success(format!(
            "thread={} agent={} workspace={}",
            context.thread_id().unwrap_or("-"),
            turn.map_or("-", |t| t.agent_id.as_str()),
            context
                .workspace_root()
                .map_or("-".to_string(), |p| p.display().to_string())
        )))
    }
}

#[tokio::test]
async fn the_adapter_runs_a_tool_under_the_in_flight_context() {
    let adapter = McpToolAdapter::new(Arc::new(ContextEcho));
    let descriptor = adapter.descriptor();
    assert_eq!(descriptor["name"], "context_echo");
    assert_eq!(descriptor["inputSchema"]["type"], "object");

    let context = InFlightContext::new(
        Some(desk_turn("ceo", &["ceo"])),
        Some(PathBuf::from("/tmp/acme/ceo")),
    );
    let result = adapter.execute(json!({}), &context).await;
    assert!(!result.is_error);
    assert_eq!(
        result.output(),
        "thread=engineering agent=ceo workspace=/tmp/acme/ceo"
    );
}

/// A speech tool goes over the server; everything else is a bare name.
///
/// The split moved. It used to be "OpenHuman's own tools are native, this
/// crate's go over MCP", and this test asserted `publish_artifact` — a
/// company tool — arriving wrapped. Now this crate's tools ride the agent's
/// belt directly (`AgentSpec::tools`), so the only thing still wrapped is
/// speech: a seat in an episode answers with one, and nothing else does.
#[test]
fn only_a_speech_tool_is_reached_through_mcp_call_tool() {
    let (name, args) =
        via_opencompany_mcp("ask", json!({ "to": "engineer", "message": "how long?" }));
    assert_eq!(name, "mcp_call_tool", "speech is the server's");
    assert_eq!(
        args,
        json!({
            "server": "opencompany",
            "tool": "ask",
            "arguments": { "to": "engineer", "message": "how long?" }
        })
    );

    // A company tool. Native since its belt became the agent's own, so the
    // model calls it by name against its own schema rather than guessing at
    // an inner `arguments` object no provider can validate.
    let (name, args) = via_opencompany_mcp("publish_artifact", json!({ "path": "memo.md" }));
    assert_eq!(name, "publish_artifact");
    assert_eq!(args, json!({ "path": "memo.md" }));

    // And an OpenHuman tool, native as it always was.
    let (name, args) = via_opencompany_mcp("file_read", json!({ "path": "memo.md" }));
    assert_eq!(name, "file_read");
    assert_eq!(args, json!({ "path": "memo.md" }));
}
