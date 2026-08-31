//! Live smoke test against a real, installed `claude-agent-acp` — not the
//! scripted fixture `acp_client.rs` drives.
//!
//! Requires `claude-agent-acp` on `PATH`
//! (`npm install -g @agentclientprotocol/claude-agent-acp`) and an
//! authenticated `claude` CLI (`claude auth status`). Costs real API /
//! subscription usage on every run, so this is `#[ignore]`d and never
//! selected by CI — run explicitly:
//!
//! ```text
//! cargo test -p opencompany-desktop --test acp_live_smoke -- --ignored --nocapture
//! ```
//!
//! Exists to validate, against the real adapter rather than the fixture, the
//! two assumptions issue #1245's harness-level `model` field depends on:
//! that `session/new` actually advertises a model-category config option or
//! the unstable `models` block, and that an env var set on the spawned
//! process actually steers which model that option reports as current.

use std::path::Path;
use std::sync::{Arc, Mutex};

use opencompany_desktop_lib::acp::client::{AcpClient, ClientHandler, ConfinedFiles};
use opencompany_desktop_lib::acp::confine::Confinement;
use serde_json::Value;

fn handler(root: &Path) -> Arc<dyn ClientHandler> {
    Arc::new(ConfinedFiles::new(
        Confinement::new(root).unwrap(),
        Some("yes".to_string()),
    ))
}

#[derive(Clone, Default)]
struct Updates(Arc<Mutex<Vec<Value>>>);

impl Updates {
    fn sink(&self) -> Arc<dyn Fn(Value) + Send + Sync> {
        let inner = Arc::clone(&self.0);
        Arc::new(move |value| inner.lock().unwrap().push(value))
    }
    fn said(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|u| u["update"]["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|u| u["update"]["content"]["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    }
}

#[tokio::test]
#[ignore = "spawns a real, authenticated claude-agent-acp and costs real usage"]
async fn a_real_claude_agent_acp_answers_a_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let updates = Updates::default();

    let client = AcpClient::spawn(
        "claude-agent-acp",
        &[],
        &root,
        &[],
        handler(&root),
        updates.sink(),
    )
    .await
    .expect(
        "claude-agent-acp must be on PATH: npm install -g @agentclientprotocol/claude-agent-acp",
    );
    client.initialize().await.expect("initialize");
    let session = client.new_session(&root).await.expect("session/new");

    let stop_reason = client
        .prompt(
            &session,
            "Reply with exactly the single word PONG and nothing else.",
        )
        .await
        .expect("prompt");
    assert_eq!(stop_reason, "end_turn", "updates were: {:?}", updates.0);

    let said = updates.said();
    assert!(said.contains("PONG"), "got: {said:?}");
}

/// Bypasses `new_session`'s narrow `sessionId`-only parsing to see the full
/// raw `session/new` response, so this can inspect `configOptions`/`models`
/// without a helper this crate doesn't have yet (that helper is #1245's job;
/// this test is what justifies building it at all).
#[tokio::test]
#[ignore = "spawns a real, authenticated claude-agent-acp and costs real usage"]
async fn session_new_advertises_a_model_config_option() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let updates = Updates::default();

    let client = AcpClient::spawn(
        "claude-agent-acp",
        &[],
        &root,
        &[],
        handler(&root),
        updates.sink(),
    )
    .await
    .expect("claude-agent-acp must be on PATH");
    client.initialize().await.expect("initialize");

    let raw = client
        .call(
            "session/new",
            serde_json::json!({ "cwd": root.display().to_string(), "mcpServers": [] }),
        )
        .await
        .expect("session/new");

    let model_option = raw["configOptions"].as_array().and_then(|opts| {
        opts.iter()
            .find(|o| o.get("category").and_then(|c| c.as_str()) == Some("model"))
    });

    // Printed, not just asserted: which model ids a harness actually
    // advertises is the input to the console's model picker, and reading them
    // off a live adapter is the only way to know them. Run with
    // `--ignored --nocapture` to see the current list.
    if let Some(option) = model_option {
        eprintln!(
            "[models] configId={:?}",
            option.get("configId").or_else(|| option.get("id"))
        );
        for value in option
            .get("options")
            .and_then(|o| o.as_array())
            .into_iter()
            .flatten()
        {
            eprintln!("[models]   {value:#}");
        }
    }

    assert!(
        model_option.is_some() || raw.get("models").is_some(),
        "expected a `configOptions` entry with category \"model\" or an unstable \
         `models` block in session/new's response, got: {raw:#}"
    );
}

/// The mechanism issue #1245's `LocalAcpAgent` will actually use: an env var
/// set on the spawned process, not a live ACP config-option switch. Runs the
/// adapter twice, under two different `ANTHROPIC_MODEL` values, and confirms
/// the reported "current" model differs — proof the env var is actually
/// consulted at startup, not silently ignored.
#[tokio::test]
#[ignore = "spawns a real, authenticated claude-agent-acp twice and costs real usage"]
async fn anthropic_model_env_var_steers_the_startup_model() {
    async fn current_model_id(model_env: &str) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let updates = Updates::default();
        let client = AcpClient::spawn(
            "claude-agent-acp",
            &[],
            &root,
            &[("ANTHROPIC_MODEL", model_env)],
            handler(&root),
            updates.sink(),
        )
        .await
        .expect("claude-agent-acp must be on PATH");
        client.initialize().await.expect("initialize");
        let raw = client
            .call(
                "session/new",
                serde_json::json!({ "cwd": root.display().to_string(), "mcpServers": [] }),
            )
            .await
            .expect("session/new");

        // Stable path: the configOptions entry whose category is "model" names
        // its current value's `currentValue` (not `configId`/`id`'s own spec
        // name of "value" — claude-agent-acp's real wire shape, confirmed
        // live). Unstable path: `models.currentModelId`.
        raw["configOptions"]
            .as_array()
            .and_then(|opts| {
                opts.iter()
                    .find(|o| o.get("category").and_then(|c| c.as_str()) == Some("model"))
            })
            .and_then(|opt| opt.get("currentValue").and_then(|v| v.as_str()))
            .map(str::to_string)
            .or_else(|| raw["models"]["currentModelId"].as_str().map(str::to_string))
    }

    let haiku = current_model_id("claude-haiku-4-5").await;
    let sonnet = current_model_id("claude-sonnet-4-5").await;

    assert!(
        haiku.is_some() && sonnet.is_some(),
        "session/new must report a current model id under ANTHROPIC_MODEL: \
         haiku={haiku:?} sonnet={sonnet:?}"
    );
    assert_ne!(
        haiku, sonnet,
        "ANTHROPIC_MODEL must actually steer the reported current model, not be ignored"
    );
}

/// The full `LocalAcpAgent` path, through the `AcpAgent` trait rather than
/// the raw `AcpClient` the tests above drive directly — the same seam
/// `harness::lanes::build` calls in production. Proves the whole chain: model
/// env-var injection, lazy session creation, and raw-JSON-to-`AcpUpdate`
/// parsing all work against the real adapter, not just each piece in
/// isolation.
#[tokio::test]
#[ignore = "spawns a real, authenticated claude-agent-acp and costs real usage"]
async fn local_acp_agent_answers_a_prompt_through_the_acp_agent_trait() {
    use opencompany::ports::acp::AcpAgentFactory;
    use opencompany::ports::types::CompanyId;
    use opencompany_desktop_lib::acp::LocalAcpAgentFactory;

    let dir = tempfile::tempdir().unwrap();
    let workspace_root = dir.path().canonicalize().unwrap();

    let agent = LocalAcpAgentFactory
        .build("claude", None, &Default::default(), &workspace_root)
        .expect("claude-agent-acp must be on PATH");

    // Watched as the real adapter reports it, which is the invariant the live
    // console timeline rests on: what an observer sees during the turn is
    // exactly what the turn returns afterwards — a tee, never a hand-off. A
    // fixture can only prove that against a fixture.
    let observed: Arc<Mutex<Vec<opencompany::ports::acp::AcpUpdate>>> = Arc::default();
    let seen = Arc::clone(&observed);
    let observer: opencompany::ports::acp::AcpObserver =
        Arc::new(move |update| seen.lock().unwrap().push(update.clone()));

    let company = CompanyId::new("acme-live-smoke");
    let turn = agent
        .prompt(
            &company,
            &format!("{}::researcher", company.as_ref()),
            "Reply with exactly the single word PONG and nothing else.",
            Some(&observer),
        )
        .await
        .expect("prompt");

    assert_eq!(
        *observed.lock().unwrap(),
        turn.updates,
        "every update the observer saw is in the returned turn, in order"
    );

    assert_eq!(
        turn.stop_reason, "end_turn",
        "updates were: {:?}",
        turn.updates
    );
    let said: String = turn
        .updates
        .iter()
        .filter_map(|u| match u {
            opencompany::ports::acp::AcpUpdate::MessageChunk(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(said.contains("PONG"), "got: {said:?}");

    // The per-agent workspace directory was created, mirroring
    // `harness::built_in::build::agent_workspace`'s layout.
    assert!(
        workspace_root
            .join("acme-live-smoke")
            .join("researcher")
            .join("workspace")
            .is_dir()
    );
}

/// `codex-acp` has no startup-model env var — confirmed by trying
/// `OPENAI_MODEL`, `CODEX_MODEL`, `MODEL` and `OPENAI_DEFAULT_MODEL` against
/// the real adapter; none moved `currentValue` off its default
/// (`gpt-5.6-sol`). It does advertise a real `configOptions` model entry
/// though, so `LocalAcpAgent` falls back to `session/set_config_option`,
/// applied once per session right after `session/new` — this proves that
/// fallback actually works, through the `AcpAgent` trait rather than the raw
/// client.
#[tokio::test]
#[ignore = "spawns a real, authenticated codex-acp and costs real usage"]
async fn local_acp_agent_steers_codex_via_set_config_option_fallback() {
    use opencompany::ports::acp::AcpAgentFactory;
    use opencompany::ports::types::CompanyId;
    use opencompany_desktop_lib::acp::LocalAcpAgentFactory;

    let dir = tempfile::tempdir().unwrap();
    let workspace_root = dir.path().canonicalize().unwrap();

    let agent = LocalAcpAgentFactory
        .build(
            "codex",
            Some("gpt-5.5"),
            &Default::default(),
            &workspace_root,
        )
        .expect("codex-acp must be on PATH");

    let company = CompanyId::new("acme-codex-smoke");
    // `prompt` is what actually opens the session and runs the fallback
    // (`session/set_config_option`) before the turn starts. Not asserting on
    // which model answered — a model's own self-reported name is not a
    // reliable oracle for the ACP-level `value` id it was switched to,
    // especially for aliased/internal names like these. What this proves is
    // that the fallback call itself is accepted by the real adapter with no
    // error: `model_config_id_matches_the_real_codex_shape` (below) already
    // proves the *parsing* deterministically, and the manual probe that
    // designed this fallback directly confirmed `currentValue` changes in
    // `session/set_config_option`'s own echoed response.
    let turn = agent
        .prompt(
            &company,
            &format!("{}::researcher", company.as_ref()),
            "Reply with exactly the single word PONG and nothing else.",
            // Unwatched: what this test is about is the model-steering
            // fallback, and the tee is asserted against the real adapter in
            // `local_acp_agent_answers_a_prompt_through_the_acp_agent_trait`.
            None,
        )
        .await
        .expect("prompt — including its session/set_config_option fallback call");

    assert_eq!(
        turn.stop_reason, "end_turn",
        "updates were: {:?}",
        turn.updates
    );
}

/// Pins `model_config_id`'s parsing against the real shape `codex-acp`
/// returns from `session/new` (captured live while designing the
/// `session/set_config_option` fallback) — including its `"id"` key rather
/// than the ACP spec's own `"configId"`, and the other, non-model config
/// options a real response carries alongside it. No process spawned, so this
/// runs in CI unlike the rest of this file.
#[test]
fn model_config_id_matches_the_real_codex_shape() {
    let raw: serde_json::Value = serde_json::from_str(
        r#"{
            "sessionId": "sess-1",
            "configOptions": [
                {
                    "id": "mode",
                    "category": "mode",
                    "currentValue": "agent",
                    "options": [{"value": "agent"}]
                },
                {
                    "id": "model",
                    "category": "model",
                    "currentValue": "gpt-5.6-sol",
                    "options": [
                        {"value": "gpt-5.6-sol"},
                        {"value": "gpt-5.5"},
                        {"value": "gpt-5.4"}
                    ]
                },
                {
                    "id": "reasoning_effort",
                    "category": "thought_level",
                    "currentValue": "medium",
                    "options": [{"value": "medium"}]
                }
            ]
        }"#,
    )
    .unwrap();

    assert_eq!(
        opencompany_desktop_lib::acp::local_agent::model_config_id(&raw, "gpt-5.5"),
        Some("model".to_string())
    );
    assert_eq!(
        opencompany_desktop_lib::acp::local_agent::model_config_id(&raw, "not-a-real-model"),
        None,
        "must not match a value the adapter never advertised"
    );
}

/// The codex twin of [`session_new_advertises_a_model_config_option`], and the
/// reason both exist rather than one: the two adapters do **not** return the
/// same shape. `codex-acp` keys its entry `id` where the ACP spec (and
/// `claude-agent-acp`) say `configId`, and it carries other categories —
/// `mode`, `thought_level` — alongside the model one. A picker built against
/// only one of them silently finds nothing on the other.
///
/// Opens a session and stops; it never prompts, so unlike
/// `local_acp_agent_steers_codex_via_set_config_option_fallback` below it runs
/// no inference. Run with `--ignored --nocapture` to see the current list.
#[tokio::test]
#[ignore = "spawns a real, authenticated codex-acp (no prompt, so no inference)"]
async fn codex_session_new_advertises_its_own_model_option_shape() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let updates = Updates::default();

    let client = AcpClient::spawn("codex-acp", &[], &root, &[], handler(&root), updates.sink())
        .await
        .expect("codex-acp must be on PATH");
    client.initialize().await.expect("initialize");

    let raw = client
        .call(
            "session/new",
            serde_json::json!({ "cwd": root.display().to_string(), "mcpServers": [] }),
        )
        .await
        .expect("session/new");

    let model_option = raw["configOptions"].as_array().and_then(|opts| {
        opts.iter()
            .find(|o| o.get("category").and_then(|c| c.as_str()) == Some("model"))
    });

    if let Some(option) = model_option {
        eprintln!(
            "[codex models] key={:?} current={:?}",
            option.get("configId").or_else(|| option.get("id")),
            option.get("currentValue")
        );
        for value in option
            .get("options")
            .and_then(|o| o.as_array())
            .into_iter()
            .flatten()
        {
            eprintln!("[codex models]   {value:#}");
        }
    }

    assert!(
        model_option.is_some(),
        "expected a model-category config option, got: {raw:#}"
    );
}

/// The whole desktop path in one call, for both adapters: `confirm` spawns the
/// harness, settles its readiness, **and** returns the models the console's
/// picker is built from — proving the two answers really do come from a single
/// spawn rather than needing a second one.
#[tokio::test]
#[ignore = "spawns the real, authenticated CLIs (no prompt, so no inference)"]
async fn confirm_returns_readiness_and_models_for_every_installed_harness() {
    for id in ["claude", "codex"] {
        let dir = tempfile::tempdir().unwrap();
        let found = opencompany_desktop_lib::acp::discovery::confirm(id, dir.path()).await;

        eprintln!("[{id}] readiness={:?}", found.readiness);
        for model in &found.models {
            eprintln!(
                "[{id}]   {}{}{}",
                model.value,
                model
                    .name
                    .as_deref()
                    .map(|n| format!("  ({n})"))
                    .unwrap_or_default(),
                if model.current { "  <- current" } else { "" },
            );
        }

        assert!(found.readiness.is_ready(), "[{id}] {:?}", found.readiness);
        assert!(
            !found.models.is_empty(),
            "[{id}] a ready harness must advertise the models its picker offers"
        );
    }
}

/// Diagnostic: exactly what `oc_acp_harnesses` returns on *this* machine.
///
/// The unit tests drive [`diagnose_absent`] through a `Fake` probe, which
/// proves the rules but says nothing about whether `SystemProbe` finds a real
/// install — the gap that made "Not installed" impossible to explain from the
/// tests alone. Run with `--ignored --nocapture` when the console and the
/// shell disagree about what is installed.
///
/// Prints what `PATH` would say about each harness *if* its adapter failed to
/// start. It is not the verdict — only running the adapter produces one, and
/// `confirm_returns_readiness_and_models_for_every_installed_harness` is the
/// test that does that — but it is where a wrong "not installed" comes from.
#[test]
#[ignore = "reports this machine's real state; asserts nothing"]
fn what_path_would_say_about_each_harness_on_this_machine() {
    use opencompany_desktop_lib::acp::discovery::{HARNESSES, SystemProbe, diagnose_absent};
    eprintln!(
        "inherited PATH = {:?}",
        std::env::var("PATH").unwrap_or_default()
    );
    eprintln!(
        "shell PATH     = {:?}",
        opencompany_desktop_lib::acp::shell_env::effective_path()
    );
    for harness in HARNESSES {
        eprintln!(
            "[{:8}] adapter={:20} would report {:?}",
            harness.id,
            harness.command,
            diagnose_absent(&SystemProbe, harness)
        );
    }
}

/// Installs an adapter the way the Install button does, then proves the thing
/// it produced actually starts and speaks ACP.
///
/// The unit tests in `acp::tools` build a `node_modules` layout by hand, which
/// proves the *reading* rules and nothing about the writing: that `npm
/// --prefix` really links the executable where this expects it, that the
/// pinned version really exists on the registry, and — the part no fixture can
/// reach — that the installed script runs on this machine. `block/buzz` shipped
/// a private tools directory whose contents could not execute (their #2342),
/// which is exactly the gap between "installed" and "works".
///
/// Uses Codex deliberately: `codex-acp` depends on `@openai/codex`, so the
/// install is self-contained and this does not also require a separately
/// installed CLI. Installs into a temp dir, never the operator's real
/// `tools_dir`.
#[tokio::test]
#[ignore = "downloads from the npm registry and spawns what it installed"]
async fn an_installed_adapter_starts_and_speaks_acp() {
    use opencompany_desktop_lib::acp::discovery::HARNESSES;
    use opencompany_desktop_lib::acp::tools;

    let harness = HARNESSES.iter().find(|h| h.id == "codex").unwrap();
    let root = tempfile::tempdir().unwrap();

    tools::install_into(root.path(), harness)
        .await
        .expect("the pinned adapter installs");

    // Where the code expects `npm --prefix` to have put things.
    let adapter = tools::installed_adapter_in(root.path(), harness)
        .expect("the executable is linked into node_modules/.bin");
    assert!(
        tools::is_pinned_version_in(root.path(), harness),
        "installed {:?}, pinned {}",
        tools::installed_version_in(root.path(), harness),
        harness.version
    );

    // The half a fixture cannot prove: it runs, and it is an ACP agent.
    let updates = Updates::default();
    let client = AcpClient::spawn(
        &adapter.display().to_string(),
        &[],
        root.path(),
        &[],
        handler(root.path()),
        updates.sink(),
    )
    .await
    .expect("the installed adapter starts");
    client
        .initialize()
        .await
        .expect("the installed adapter completes the ACP handshake");

    eprintln!("[install] {} -> {}", harness.version, adapter.display());
}

/// The assumption durable session continuity rests on: that
/// `agentCapabilities.loadSession` is real, that a session id outlives the
/// process that opened it, and that the resumed conversation still carries
/// what was said before the restart.
///
/// Runs the adapter **twice** — the second time against a session the first
/// one opened, with the first process gone — which is exactly the shape of an
/// operator restarting the app between two questions to the same teammate.
#[tokio::test]
#[ignore = "spawns a real, authenticated claude-agent-acp twice and costs real usage"]
async fn a_session_survives_the_process_that_opened_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    let session = {
        let updates = Updates::default();
        let client = AcpClient::spawn(
            "claude-agent-acp",
            &[],
            &root,
            &[],
            handler(&root),
            updates.sink(),
        )
        .await
        .expect("claude-agent-acp must be on PATH");
        let hello = client.initialize().await.expect("initialize");

        // Read, never assumed: an adapter that cannot resume must get a fresh
        // `session/new` rather than a `session/load` that fails every cold
        // start. Printed because this is the field the whole feature is gated
        // on, and it is worth seeing when it changes.
        eprintln!(
            "[loadSession] {:?}",
            hello["agentCapabilities"]["loadSession"]
        );
        assert_eq!(
            hello["agentCapabilities"]["loadSession"].as_bool(),
            Some(true),
            "claude-agent-acp advertises loadSession; got: {hello:#}"
        );

        let session = client.new_session(&root).await.expect("session/new");
        client
            .prompt(
                &session,
                "The codeword for this conversation is `pomegranate`. \
                 Reply with exactly: OK. Do not use any tools.",
            )
            .await
            .expect("prompt");
        session
        // The client drops here, and with it the subprocess (`kill_on_drop`) —
        // the restart this test is about.
    };

    let updates = Updates::default();
    let client = AcpClient::spawn(
        "claude-agent-acp",
        &[],
        &root,
        &[],
        handler(&root),
        updates.sink(),
    )
    .await
    .expect("claude-agent-acp must be on PATH");
    client.initialize().await.expect("initialize");

    client
        .call(
            "session/load",
            serde_json::json!({
                "sessionId": session,
                "cwd": root.display().to_string(),
                "mcpServers": [],
            }),
        )
        .await
        .expect("session/load reopens a session opened by a dead process");

    client
        .prompt(
            &session,
            "What was the codeword I gave you? Answer in one word. Do not use any tools.",
        )
        .await
        .expect("prompt");

    let said = updates.said().to_lowercase();
    assert!(
        said.contains("pomegranate"),
        "the resumed conversation still carries what was said before the restart; got: {said:?}"
    );
}

/// The failure the fallback is written for: an id the adapter no longer holds
/// — a cleared CLI session store, a record copied between machines — is
/// refused, not silently answered with an empty session that would look like
/// a teammate whose memory went blank.
///
/// Cheap enough to be worth its own test: no model call, so this costs
/// nothing but a spawn.
#[tokio::test]
#[ignore = "spawns a real claude-agent-acp"]
async fn an_unknown_session_is_refused_rather_than_invented() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let updates = Updates::default();

    let client = AcpClient::spawn(
        "claude-agent-acp",
        &[],
        &root,
        &[],
        handler(&root),
        updates.sink(),
    )
    .await
    .expect("claude-agent-acp must be on PATH");
    client.initialize().await.expect("initialize");

    let refused = client
        .call(
            "session/load",
            serde_json::json!({
                "sessionId": "00000000-0000-4000-8000-000000000000",
                "cwd": root.display().to_string(),
                "mcpServers": [],
            }),
        )
        .await;

    let error = refused.expect_err("an unknown session id is not loadable");
    eprintln!("[session/load unknown] {error}");
}
