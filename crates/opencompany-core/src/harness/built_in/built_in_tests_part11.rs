//! Regression tests for issue #1871 and the two CodeRabbit majors on PR #2421
//! (review comments 4084652854 and 4084652871).
//!
//! #1871: a blank first completion takes the `AttemptOutcome::Empty` arm and is
//! retried once; the failed attempt must not leak into the retry — no duplicate
//! user message in the model context, no failure rows in the durable
//! transcript.
//!
//! The two review findings were written against the pre-refactor pin, where
//! `Agent::turn` appended the user row on entry and `record_failed_turn`
//! committed the failed attempt (user row + failure note) to the durable
//! transcript, so the retry flow rolled history back by flattening typed rows
//! into `(role, content)` pairs and re-seeding:
//!
//! * 4084652854 — that rollback mapped retained `system` rows to `user` and
//!   dropped `AssistantToolCalls`/`ToolResults` rows outright.
//! * 4084652871 — the rollback touched only memory; the durable transcript
//!   kept the failed attempt, so model-context and display reads disagreed.
//!
//! At the current OpenHuman pin the session runtime owns both projections and
//! a turn is transactional: `Session::turn` builds the provider input from a
//! *clone* of the committed history and, when the driver returns
//! `EmptyProviderResponse`, the failure carries no partial history — nothing
//! is appended to `self.history` and nothing is persisted. Every turn also
//! re-resumes the durable transcript from disk (`ResumeMode::Session`), so the
//! retry's context is exactly the pre-attempt state, with system rows and
//! typed tool rows replayed losslessly from the raw rows on disk. There is no
//! rollback left to flatten or to drift: one logical turn lands as one user
//! message plus the recovered reply in BOTH projections.
//!
//! These tests pin that contract end to end, so a future vendored bump that
//! reintroduces per-attempt writes fails here instead of reaching review:
//!
//! 1. the retry arm is reachable from one ordinary blank completion;
//! 2. the retry does not duplicate the user message in the model context;
//! 3. both attempts of a retried turn send the same tool scope;
//! 4. the failed attempt leaves no durable trace, and the on-disk display
//!    companion agrees with the durable rows;
//! 5. a later turn on the same session replays the durable rows with system
//!    and assistant roles intact;
//! 6. typed tool-call / tool-result rows survive the replay across the retry.

use super::built_in_test_fixtures::*;
use super::built_in_test_fixtures_2::*;
use super::*;

/// **Reachability** — a single `Ok(String::new())` in the scripted-provider
/// sequence causes the wrapper to make exactly two provider calls: the first
/// ends in the driver's empty-response error (blank text, no tool outcomes),
/// which `classify_turn` reads as `AttemptOutcome::Empty`, and the second
/// consumes the recovery reply.
#[tokio::test]
async fn a_single_blank_script_reaches_the_empty_retry_arm() {
    let (agent, _deps) = scripted_agent(vec![Ok(String::new()), Ok("recovery".into())]);
    let (outcome, usages) = agent.run("hello").await;
    let outcome = outcome.expect("wrapper recovers");
    assert!(
        outcome.reply.contains("recovery"),
        "second attempt reply must reach the caller: {:?}",
        outcome.reply,
    );
    assert_eq!(
        usages.len(),
        2,
        "exactly two provider calls must have been made — one for the blank, one for the \
         recovery: {usages:?}",
    );
}

/// **No duplication in the model context** — the retry's provider request must
/// carry exactly the message kinds the first attempt carried: the failed
/// attempt committed nothing, so the second request is built from the same
/// pre-attempt history. Before #1871 was fixed, the second request carried one
/// extra user row; the pre-refactor rollback on PR #2421 then lost `system`
/// rows to `user` flattening — both show up here as a roles-sequence mismatch.
#[tokio::test]
async fn an_empty_retry_does_not_duplicate_the_user_message_in_history() {
    let (agent, _deps, capture) =
        scripted_agent_with_capture(vec![Ok(String::new()), Ok("reply".into())]);
    let (outcome, _usages) = agent.run("hello").await;
    outcome.expect("wrapper recovers");

    let calls = capture.captured.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        2,
        "two provider calls (blank + recovery): {calls:?}"
    );
    let first_users = calls[0].count(CapturedRole::User);
    let second_users = calls[1].count(CapturedRole::User);
    assert_eq!(
        first_users, second_users,
        "the retry must NOT duplicate the user message — both provider requests must \
         carry the same number of user-role messages: {calls:?}",
    );
    assert!(
        first_users >= 1,
        "at least one user message must be present in each request: {calls:?}",
    );
    assert_eq!(
        calls[0].roles, calls[1].roles,
        "the retry must resend the pre-attempt context verbatim — same message kinds in \
         the same order, with system rows still system: {calls:?}",
    );
}

/// **Scope parity** — both attempts of a retried turn must send the same tool
/// surface: the per-turn scope is decided once, before the first attempt, and
/// the runtime holds no per-attempt mutation of it.
#[tokio::test]
async fn an_empty_retry_keeps_the_same_tool_scope_on_both_attempts() {
    let (agent, _deps, capture) =
        scripted_agent_with_capture(vec![Ok(String::new()), Ok("reply".into())]);
    let (outcome, _usages) = agent.run("do the task").await;
    outcome.expect("wrapper recovers");

    let calls = capture.captured.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        2,
        "two provider calls (blank + recovery): {calls:?}"
    );
    assert_eq!(
        calls[0].tools, calls[1].tools,
        "the same tool declarations, in the same order, must go out on both attempts: {calls:?}",
    );
    assert!(
        !calls[0].tools.is_empty(),
        "a normal turn must send at least one tool — if this fails the fixture lost its \
         toolbelt and the parity assertion above is vacuous: {calls:?}",
    );
}

/// Recursive helper: every `*.jsonl` transcript under `dir`, as `(path, body)`.
fn transcripts_under(dir: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Ok(body) = std::fs::read_to_string(&path)
            {
                out.push((path, body));
            }
        }
    }
    out
}

/// Recursive helper: every `*.md` display companion under `dir` whose body
/// contains `marker`.
fn display_companions_containing(dir: &std::path::Path, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md")
                && let Ok(body) = std::fs::read_to_string(&path)
                && body.contains(marker)
            {
                out.push(body);
            }
        }
    }
    out
}

/// The durable message rows of one transcript body: parsed JSONL lines that
/// carry a `role`, as `(role, content)`; `_meta` headers and tool-snapshot
/// records have no `role` and are skipped.
fn durable_rows(body: &str) -> Vec<(String, String)> {
    body.lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            let role = value.get("role")?.as_str()?.to_string();
            let content = value
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string();
            Some((role, content))
        })
        .collect()
}

/// **No durable trace, and the two projections agree** — after a blank first
/// attempt and a successful retry, the session's durable transcript holds
/// exactly one logical turn: one user row plus the recovered reply. No
/// failure note, no duplicate user row — and the rendered display companion
/// (the `.md` projection the session writer re-renders on every append)
/// carries the same content.
///
/// This is review comment 4084652871's contract pinned: the pre-refactor flow
/// persisted the failed attempt through `record_failed_turn` and the retry
/// then wrote a compaction replacement, so the display kept the failed
/// attempt but not the recovery reply. At this pin the failed attempt never
/// reaches the disk at all, so the disagreement has no source.
#[tokio::test]
async fn the_failed_attempt_leaves_no_durable_trace_and_display_matches() {
    let marker = format!("durable-1871-{}", uuid::Uuid::new_v4().simple());
    let (agent, _deps) = scripted_agent(vec![Ok(String::new()), Ok("recovered reply".into())]);
    let (outcome, _usages) = agent.run(&marker).await;
    outcome.expect("wrapper recovers");

    let root = test_runtime().root_dir().to_path_buf();
    let hits: Vec<_> = transcripts_under(&root)
        .into_iter()
        .filter(|(_, body)| body.contains(&marker))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "exactly one durable transcript may carry this turn's message: {hits:?}",
    );
    let rows = durable_rows(&hits[0].1);
    let user_rows: Vec<_> = rows
        .iter()
        .filter(|(role, content)| role == "user" && content.contains(&marker))
        .collect();
    assert_eq!(
        user_rows.len(),
        1,
        "the durable transcript must hold the user message exactly once: {rows:?}",
    );
    assert!(
        rows.iter()
            .any(|(role, content)| role == "assistant" && content.contains("recovered reply")),
        "the recovery reply must be the durable assistant row: {rows:?}",
    );
    for (role, content) in &rows {
        let lowered = content.to_lowercase();
        assert!(
            !lowered.contains("empty response") && !lowered.contains("turn failed"),
            "no failure note may persist ({role}: {content}): {rows:?}",
        );
    }

    let companions = display_companions_containing(&root, &marker);
    assert_eq!(
        companions.len(),
        1,
        "the display companion must render this turn exactly once: {companions:?}",
    );
    let display = &companions[0];
    assert!(
        display.contains("recovered reply"),
        "the display projection must render the recovery reply, not just a marker: {display}",
    );
    let lowered = display.to_lowercase();
    assert!(
        !lowered.contains("empty response") && !lowered.contains("turn failed"),
        "the display projection must not render the failed attempt: {display}",
    );
}

/// **Durable replay keeps roles** — a second turn on the same conversation
/// session rebuilds its model context from the durable transcript on disk
/// (the runtime re-resumes the session file on every turn), so the request it
/// sends is the first turn's rows — system rows still `system`, the first
/// user row still `user`, the recovery reply still `assistant` — plus exactly
/// one new user row. This is review comment 4084652854's contract pinned from
/// the provider's side: no retained row may arrive flattened to `user`.
#[tokio::test]
async fn a_later_turn_replays_the_durable_rows_with_roles_intact() {
    let (agent, _deps, capture) = scripted_agent_with_capture(vec![
        Ok(String::new()),
        Ok("recovery".into()),
        Ok("follow-up".into()),
    ]);
    let chat = crate::runtime::delegation::ChatTarget {
        chat_id: Some("desk-1871-replay"),
        thread_root: None,
        history_seed: true,
        message_seq: None,
    };
    let (first, _usages) = agent.run_with_steer("hello", None, None, None, chat).await;
    first.expect("first turn recovers");
    let (second, _usages) = agent
        .run_with_steer("and again", None, None, None, chat)
        .await;
    second.expect("follow-up turn succeeds");

    let calls = capture.captured.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        3,
        "three provider calls (blank + recovery + follow-up): {calls:?}",
    );
    // The first attempt's request is [system…, user]; the follow-up turn's
    // request must be exactly that plus the replayed recovery reply and the
    // new user row — nothing flattened, nothing dropped, nothing extra.
    let mut expected = calls[0].roles.clone();
    expected.push(CapturedRole::Assistant);
    expected.push(CapturedRole::User);
    assert_eq!(
        calls[2].roles, expected,
        "the follow-up turn must replay the durable rows with their original roles: \
         {calls:?}",
    );
    assert!(
        calls[0].count(CapturedRole::System) >= 1,
        "the fixture must send a system row, or the retention assertion is vacuous: \
         {calls:?}",
    );
}

// ---------------------------------------------------------------------------
// Typed-row retention across the retry, on the production native-tool path
// ---------------------------------------------------------------------------
//
// The scripted provider above answers prose only; native tool calls need the
// provider profile that advertises `tool_calling`, so this half drives a real
// `HostedProvider` against a scripted OpenAI-compatible endpoint on loopback —
// the seam `iteration_cap_turn_tests` established — with one boundary stubbed
// (the model's choices) and everything else real: policy, sandboxed file
// tools, session persistence.

/// What the scripted model does on each successive call.
enum WireTurn {
    /// Emit a tool call with these literal arguments.
    Call {
        tool: String,
        args: serde_json::Value,
    },
    /// Finish the turn with plain assistant text (possibly blank).
    Say(&'static str),
}

/// A scripted OpenAI-compatible `/chat/completions` endpoint.
struct WireScript {
    turns: std::sync::Mutex<Vec<WireTurn>>,
    /// Every request body the harness sent, for post-hoc assertions.
    seen: std::sync::Mutex<Vec<serde_json::Value>>,
}

/// One assistant message carrying a native `tool_calls` array — the shape the
/// provider's `tool_calling: true` profile puts the turn loop on.
fn wire_tool_call_message(tool: &str, args: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": format!("call-{tool}"),
            "type": "function",
            "function": { "name": tool, "arguments": args.to_string() }
        }]
    })
}

/// Serve the script on loopback and return its base URL plus the shared handle.
async fn spawn_wire_script(turns: Vec<WireTurn>) -> (String, Arc<WireScript>) {
    use axum::routing::post;

    let script = Arc::new(WireScript {
        turns: std::sync::Mutex::new(turns),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let handle = Arc::clone(&script);
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let script = Arc::clone(&handle);
            async move {
                script.seen.lock().unwrap().push(body.clone());
                let next = {
                    let mut turns = script.turns.lock().unwrap();
                    if turns.is_empty() {
                        None
                    } else {
                        Some(turns.remove(0))
                    }
                };
                // Running off the end of the script means the turn looped more
                // than expected; end it with text rather than hanging.
                let next = next.unwrap_or(WireTurn::Say("ran off the end of the script"));
                let message = match next {
                    WireTurn::Say(text) => {
                        serde_json::json!({ "role": "assistant", "content": text })
                    }
                    WireTurn::Call { tool, args } => wire_tool_call_message(&tool, &args),
                };
                axum::Json(serde_json::json!({
                    "choices": [{ "index": 0, "message": message }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 4 }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), script)
}

/// One real company agent on the scripted endpoint, with `notes` files seeded
/// in its sandbox so every scripted `file_read` succeeds.
async fn wire_company_agent(
    model_url: String,
    dir: &std::path::Path,
    notes: usize,
) -> CompanyAgent {
    use crate::company::credentials::Credential;
    use crate::company::{Agent as ManifestAgent, Policy};
    use crate::harness::build::{agent_workspace, build_agent};
    use crate::harness::policy::ApprovalPolicy;
    use crate::harness::provider::{HostedProvider, HostedProviderConfig};
    use crate::store::{FsCompanyStore, FsContextStore};

    let deps = HarnessDeps {
        takeovers: Default::default(),
        emergency_gate: None,
        notifications: None,
        ledgers: None,
        ledger_registry: Default::default(),
        provider: Arc::new(HostedProvider::new(HostedProviderConfig {
            base_url: model_url,
            credential: Credential::from_value("stub-key"),
            extra_headers: Vec::new(),
        })),
        provider_slug: "managed".to_string(),
        serves: None,
        context: Arc::new(FsContextStore::new(dir)),
        store: Arc::new(FsCompanyStore::new(dir)),
        meter: None,
        workspace_root: dir.to_path_buf(),
        mcp_home: None,
        workspace_git_enabled: false,
        audit_root: dir.to_path_buf(),
        model_override: Some("stub-model".to_string()),
        tasks: None,
        artifacts: None,
        skills: None,
        skills_source_dir: None,
        skills_registry: std::sync::Arc::from([]),
        default_mcp_servers: Vec::new(),
        mcp_servers: Vec::new(),
        facts: None,
        events: None,
        delegations: crate::harness::orchestrator::DelegationQueue::default(),
        workflow_runner: crate::harness::orchestrator::WorkflowRunnerHandle::default(),
        mcp_failures: crate::harness::mcp_probe::McpFailureQueue::default(),
        pending_publishes: crate::harness::publish::PendingPublishQueue::default(),
        workflow_refs: crate::harness::workflow_refs::WorkflowRefQueue::default(),
        run_outputs: crate::harness::orchestrator::RunOutputCache::default(),
        run_output_store: None,
        workflow_revisions: None,
        workflow_runs: None,
        deep_trace: None,
        approval_requests: crate::harness::policy::ApprovalRequestQueue::default(),
        approval_parker: None,
        secrets: None,
        web_allowed_domains: Vec::new(),
        capabilities: crate::harness::toolbelt::CapabilityFilter::AllowAll,
        workflow_source_dir: None,
        plan: None,
        media: None,
        composio: None,
        #[cfg(feature = "chargebee")]
        chargebee: None,
        #[cfg(feature = "paypal")]
        paypal: None,
        hosting: None,
        steer: crate::company::steer::InflightRegistry::default(),
        run_supervisor: crate::runtime::RunSupervisor::default(),
        delivery: None,
        search: None,
        tenant_search: None,
        workspace: None,
    };
    // A fresh id per fixture: one test binary registers this agent many times
    // over, and a runtime id stays taken while a prior fixture's handle lives.
    // The same id must carry the workspace seeding, the build, and the
    // registration, or the agent resolves an empty workspace.
    let company = crate::ports::CompanyId::new(format!("test-{}", uuid::Uuid::new_v4().simple()));
    let manifest_agent = ManifestAgent {
        provider: None,
        global: false,
        id: "ceo".to_string(),
        role: "Chief Executive".to_string(),
        name: None,
        description: None,
        tier: None,
        tools: None,
        delegates_to: Vec::new(),
        context: None,
        harness: None,
        budget_usd_daily: None,
        prompt: None,
        prompt_files: Vec::new(),
        prompt_files_resolved: Vec::new(),
        classes: Vec::new(),
        ledgers: None,
        can_declare_ledgers: true,
        model: None,
    };
    // The manifest default. `file_read` reaches nothing outside the sandbox,
    // so it is auto-approved here — the point is that the turn is gated by the
    // real policy, not that the policy is switched off for the test.
    let policy = ApprovalPolicy::new(&Policy::default(), None);
    let agent = build_agent(
        &company,
        "Acme",
        &manifest_agent,
        std::sync::Arc::new(policy),
        &deps,
        &["docs".to_string()],
        &[],
        &[],
        None,
        false,
    )
    .expect("agent builds");

    let workspace = agent_workspace(&deps.workspace_root, &company, "ceo");
    std::fs::create_dir_all(&workspace).expect("workspace");
    for i in 0..notes {
        std::fs::write(
            workspace.join(format!("note-{i:02}.md")),
            format!("Note {i}.\n"),
        )
        .expect("seed note");
    }

    let runtime = crate::harness::openhuman_runtime::global(
        crate::harness::openhuman_runtime::RuntimeBoot::ephemeral(),
    )
    .await
    .expect("the OpenHuman runtime boots");
    CompanyAgent::register(
        &runtime,
        &company,
        "ceo",
        "Chief Executive",
        None,
        agent,
        None,
    )
    .expect("the agent registers")
}

/// **Typed rows survive** — a turn that runs a real tool commits
/// assistant-tool-call and tool-result rows to the durable transcript; when a
/// later turn on the same session blanks and retries, the retry's provider
/// request must replay those rows TYPED: system rows as `system`, the tool
/// call as an assistant `tool_calls` message, the result as a `tool` message,
/// plus exactly one new `user` row. The pre-refactor rollback this replaces
/// dropped both typed variants and remapped system rows to `user` (review
/// comment 4084652854); the durable file and its display companion must show
/// the same two logical turns with no failure note (comment 4084652871).
#[tokio::test]
async fn typed_tool_rows_survive_the_replay_across_an_empty_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (url, script) = spawn_wire_script(vec![
        WireTurn::Call {
            tool: "file_read".to_string(),
            args: serde_json::json!({ "path": "note-00.md" }),
        },
        WireTurn::Say("turn-one answer"),
        WireTurn::Say(""),
        WireTurn::Say("recovered answer"),
    ])
    .await;
    let agent = wire_company_agent(url, dir.path(), 1).await;
    let chat = crate::runtime::delegation::ChatTarget {
        chat_id: Some("desk-1871-typed"),
        thread_root: None,
        history_seed: true,
        message_seq: None,
    };

    let marker1 = format!("typed-1871-first-{}", uuid::Uuid::new_v4().simple());
    let (first, _usages) = agent.run_with_steer(&marker1, None, None, None, chat).await;
    first.expect("the tool turn completes");
    let marker2 = format!("typed-1871-second-{}", uuid::Uuid::new_v4().simple());
    let (second, _usages) = agent.run_with_steer(&marker2, None, None, None, chat).await;
    let second = second.expect("the retried turn recovers");
    assert!(
        second.reply.contains("recovered answer"),
        "the retry's reply must reach the caller: {:?}",
        second.reply,
    );

    // Four model calls: the tool call, turn one's answer, the blank, the
    // recovery. The recovery request is the one under test.
    let seen = script.seen.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        4,
        "four model calls (tool call, answer, blank, recovery): {}",
        seen.len(),
    );
    let retry = &seen[3];
    let messages = retry
        .get("messages")
        .and_then(|m| m.as_array())
        .expect("the request carries messages");
    // A nested fn, not a closure: elision gives each call its own borrow,
    // where a closure's inferred lifetime is too tight for the compiler.
    fn role_of(message: &serde_json::Value) -> &str {
        message
            .get("role")
            .and_then(|role| role.as_str())
            .unwrap_or("")
    }
    let count_role = |role: &str| messages.iter().filter(|m| role_of(m) == role).count();
    assert!(
        count_role("system") >= 1,
        "system rows must reach the provider as system rows, not flattened to user: \
         {messages:?}",
    );
    assert_eq!(
        count_role("user"),
        2,
        "one user message per logical turn — the failed attempt added none: {messages:?}",
    );
    let tool_call_rows = messages
        .iter()
        .filter(|m| {
            role_of(m) == "assistant"
                && m.get("tool_calls")
                    .and_then(|calls| calls.as_array())
                    .is_some_and(|calls| !calls.is_empty())
        })
        .count();
    assert_eq!(
        tool_call_rows, 1,
        "the assistant tool-call row must be replayed typed, not dropped or flattened: \
         {messages:?}",
    );
    assert_eq!(
        count_role("tool"),
        1,
        "the tool-result row must be replayed as a tool message: {messages:?}",
    );
    assert_eq!(
        role_of(messages.last().expect("a last message")),
        "user",
        "the new user line closes the request: {messages:?}",
    );
    let wire = retry.to_string().to_lowercase();
    assert!(
        !wire.contains("empty response") && !wire.contains("turn failed"),
        "no failure note may reach the model context: {wire}",
    );

    // The durable side: this conversation's one transcript holds both logical
    // turns, typed, and its display companion agrees.
    let runtime = crate::harness::openhuman_runtime::existing().expect("runtime booted");
    let root = runtime.root_dir().to_path_buf();
    let hits: Vec<_> = transcripts_under(&root)
        .into_iter()
        .filter(|(_, body)| body.contains(&marker1) && body.contains(&marker2))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "both logical turns must land in one session transcript: {hits:?}",
    );
    let body = &hits[0].1;
    let rows = durable_rows(body);
    for marker in [&marker1, &marker2] {
        let user_rows = rows
            .iter()
            .filter(|(role, content)| role == "user" && content.contains(marker.as_str()))
            .count();
        assert_eq!(
            user_rows, 1,
            "each logical turn's user row must persist exactly once: {rows:?}",
        );
    }
    assert!(
        body.contains("file_read"),
        "the assistant tool-call row must persist: {body}",
    );
    assert!(
        body.contains("turn-one answer") && body.contains("recovered answer"),
        "both replies must persist: {body}",
    );
    let lowered = body.to_lowercase();
    assert!(
        !lowered.contains("empty response") && !lowered.contains("turn failed"),
        "no failure note may persist: {body}",
    );
    let companions = display_companions_containing(&root, &marker2);
    assert_eq!(
        companions.len(),
        1,
        "the display companion must render the retried turn exactly once: {companions:?}",
    );
    let display = &companions[0];
    assert!(
        display.contains("recovered answer") && display.contains(&marker1),
        "the display projection must show both logical turns: {display}",
    );
    let lowered = display.to_lowercase();
    assert!(
        !lowered.contains("empty response") && !lowered.contains("turn failed"),
        "the display projection must not render the failed attempt: {display}",
    );
}
