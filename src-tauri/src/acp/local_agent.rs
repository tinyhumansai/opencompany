//! `LocalAcpAgent`: the `transport = "local"` implementation of the host
//! crate's [`AcpAgent`] port (issue #1245) — a real coding CLI, spawned once
//! per declared local-acp harness and driven over stdio through the existing
//! [`AcpClient`].
//!
//! ## One process, many sessions
//!
//! A harness can serve more than one teammate, but [`AcpClient::spawn`] opens
//! one subprocess with one global update sink — ACP's `session/update`
//! notifications are not routed per caller, only tagged with the `sessionId`
//! they belong to. So this buffers every notification by `sessionId` as it
//! arrives, and a `prompt` call drains only its own session's buffer after
//! `session/prompt` returns rather than reading whatever the sink last saw.
//!
//! **One turn at a time per session, enforced rather than assumed.** Both that
//! drain and the live observer registered beside it key on `sessionId`, so two
//! concurrent turns on the *same* (company, agent) pair would interleave their
//! updates into one drain and one observer — the second registration
//! displacing the first, and the first's teardown then silencing the second.
//!
//! This file used to state the single-turn property as an assumption about
//! callers. It is not one they honour: a workflow's parallel gate can fan out
//! to two sibling nodes bound to the same teammate, and a workflow node can
//! overlap a chat turn (PR #1904 review). So [`LocalAcpAgent::session_lock`]
//! makes it true instead — one prompt at a time per session, which is also
//! what a *conversation* means. Two turns for one teammate now queue rather
//! than corrupt each other's transcript.
//!
//! Two *different* teammates on one harness are unaffected: they hold
//! different sessions and different locks, and the demultiplexing above is
//! what keeps their updates apart.
//!
//! ## Sessions outlive the process that opened them
//!
//! The `session_key` → `sessionId` map here is in memory, and a fresh
//! [`LocalAcpAgent`] is built on every runtime rebuild — a manifest edit, an
//! inference-settings change, an app restart. So the map alone meant a
//! teammate's conversation silently started over, with nothing on the
//! operator's screen to say the memory had gone.
//!
//! [`LocalAcpAgent::session_record_path`] writes the id down and
//! [`LocalAcpAgent::resume_session`] reopens it with ACP's own `session/load`,
//! capability-gated on what the adapter advertised at `initialize` and falling
//! back to `session/new` on every failure. See
//! `docs/spec/runtime/harnesses.md`.
//!
//! ## Permission requests: copied from `buzz-agent`, not bridged to the queue
//!
//! An earlier draft routed ACP `session/request_permission` calls through
//! `ApprovalRequestQueue` and, until that landed, refused every request by
//! default. `buzz-agent` (`crates/buzz-acp`) answers a much simpler question
//! instead — trust the CLI's own permission mode, and auto-approve whatever
//! it still asks about — and that is what this does too, via
//! [`AutoApprovingFiles`]. There is no human-approval queue in the loop here;
//! an ACP-run teammate's own CLI is the trust boundary, the same as it is for
//! a developer running that CLI interactively themselves.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use opencompany::Result;
use opencompany::error::OpenCompanyError;
use opencompany::ports::acp::{AcpAgent, AcpAgentFactory, AcpTurn, AcpUpdate};
use opencompany::ports::types::CompanyId;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use opencompany::ports::acp::AcpObserver;

use crate::acp::client::{AcpClient, AutoApprovingFiles, ClientHandler, ConfinedFiles};
use crate::acp::confine::Confinement;
use crate::acp::discovery::HARNESSES;
use crate::acp::discovery::Harness;

/// Bound on a tool call's `title` as it reaches this host.
///
/// A `title` is unvalidated text from an external agent process, and it
/// travels further than a log line: it becomes a [`TurnStep`] label on the
/// operator's durable timeline *and*, since execution state started
/// streaming, a live frame on every watching console. The port already
/// promised the transport bounds what it hands up ("a short summary … already
/// bounded by the transport") — this is that promise kept. Sized for a
/// recognisable label, not a paragraph.
///
/// [`TurnStep`]: opencompany::ports::types::TurnStep
const MAX_TITLE_CHARS: usize = 200;

/// Bound on a tool call's result summary, same reasoning as
/// [`MAX_TITLE_CHARS`] and larger because a result legitimately carries more:
/// a file's shape, a command's tail, an error's cause. Still small enough
/// that a runaway tool cannot flood the timeline or the live bus.
const MAX_RESULT_CHARS: usize = 2_000;

/// `text`, cut to at most `max` **characters** (never bytes — a byte slice can
/// split a UTF-8 sequence and panic) plus a trailing ellipsis when it was cut —
/// so a cut result is `max + 1` characters, not `max`. The ellipsis is a
/// visible marker that truncation happened, not part of the budget it marks.
fn bounded(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// Per-CLI startup model env var, confirmed live against the real adapter
/// (issue #1245's live smoke test) — not guessed. `None` means this build has
/// no known startup env var for that CLI, and [`LocalAcpAgent::session_for`]
/// falls back to the ACP-native `session/set_config_option` path instead —
/// also confirmed live, for `codex-acp` specifically (its `configOptions`
/// model entry accepts a set; no env var candidate tried had any effect).
fn model_env_var(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some("ANTHROPIC_MODEL"),
        _ => None,
    }
}

/// One spawned local-transport ACP harness, serving every teammate bound to
/// it.
pub struct LocalAcpAgent {
    /// The catalogue entry this agent drives.
    ///
    /// Deliberately *not* a resolved path. Resolution happens in
    /// [`Self::client`], at the moment of spawn, because the answer changes
    /// while this value is alive: a runtime is built when the company boots,
    /// the operator presses Install afterwards, and a path snapshotted at
    /// construction would still name whatever was there before — so the probe
    /// would report `Ready` off the newly installed adapter while every real
    /// turn kept spawning the old one until a restart.
    harness: &'static Harness,
    args: Vec<String>,
    env: Vec<(String, String)>,
    /// The desired model, kept regardless of whether an env var already
    /// carries it — [`Self::session_for`] falls back to
    /// `session/set_config_option` when [`model_env_var`] returned `None` at
    /// construction, so this is the only record of what was actually asked
    /// for in that case.
    model: Option<String>,
    /// Per-agent model overrides for the teammates this harness serves,
    /// keyed by agent id (issue #1245's per-agent follow-up). An agent absent
    /// here takes [`Self::model`], the harness's own default, unchanged.
    /// Always attempted via `session/set_config_option` in
    /// [`Self::session_for`] regardless of [`Self::env`] — unlike the
    /// harness-level model, an override cannot be satisfied by the shared
    /// subprocess's env, since two agents on one harness share that process.
    agent_models: HashMap<String, String>,
    /// The company's agent-workspace root (`HarnessDeps::workspace_root`).
    /// Each session roots at `workspace_root/<company>/<agent>/workspace`,
    /// mirroring `harness::built_in::build::agent_workspace` exactly, so an
    /// ACP-run teammate's files land in the same conventional place a
    /// `built_in`-run one's would.
    workspace_root: PathBuf,
    client: AsyncMutex<Option<Arc<AcpClient>>>,
    /// `session_key` (`"{company}::{agent_id}"`) → ACP `sessionId`.
    sessions: AsyncMutex<HashMap<String, String>>,
    /// One lock per ACP `sessionId`, held for the length of a turn — see the
    /// module docs. Handed out by [`LocalAcpAgent::session_lock`] and kept for
    /// the agent's life: a session is few and long-lived, and dropping a lock
    /// between turns would let a queued turn take a fresh one and race the
    /// turn it was supposed to wait for.
    session_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// `session/update` notifications, demultiplexed by ACP `sessionId` —
    /// see the module docs for why this exists at all.
    pending_updates: Arc<StdMutex<HashMap<String, Vec<Value>>>>,
    /// The live observer watching each session's **in-flight** turn, keyed by
    /// ACP `sessionId`.
    ///
    /// Registered for the window of one `session/prompt` and removed when it
    /// ends — including when the turn future is *dropped*, which the steered
    /// path does to a turn that ignored its cancel (see [`LiveTurn`]). The
    /// window matters: `session/load` replays a resumed conversation as
    /// ordinary `session/update` notifications *before* any turn starts, and
    /// an observer registered for the agent's lifetime would republish that
    /// history as this turn's execution state.
    live: Arc<StdMutex<HashMap<String, AcpObserver>>>,
    /// Whether the spawned adapter advertised `agentCapabilities.loadSession`
    /// in its `initialize` response.
    ///
    /// Read once at spawn and cached, because that is the only time it is
    /// offered. `false` until a client exists, which is the safe default: it
    /// only ever gates *attempting* `session/load`, and attempting it on an
    /// adapter that does not implement it would trade a working new session
    /// for a "method not found".
    load_session: std::sync::atomic::AtomicBool,
}

/// What is written down about a (company, agent) pair's conversation, so a
/// later process can pick it back up.
///
/// No timestamp on purpose: the adapter is the authority on whether a session
/// still exists, and it answers that question definitively on `session/load`.
/// A local "probably too old" heuristic could only ever throw away a session
/// the adapter would happily have loaded.
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionRecord {
    /// Which catalogue harness minted it — see [`LocalAcpAgent::read_session_record`].
    harness: String,
    #[serde(rename = "sessionId")]
    session_id: String,
    /// The model this session was left on, as far as this host knows —
    /// whether an env var carried it at spawn or a
    /// `session/set_config_option` applied it afterwards. `None` means no
    /// model was ever chosen and the session runs on the adapter's own
    /// default.
    ///
    /// Recorded because a resumed session restores its **session-scoped**
    /// model config, and this is the only way to notice that the config no
    /// longer matches what the company asks for (PR #1904 review). Absent
    /// from records written before this field existed, which `serde` reads
    /// as `None` — the same value a never-configured session has, so an old
    /// record resumes exactly as it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

/// Registers a live observer for one turn and removes it on drop.
///
/// A guard rather than a matching pair of calls because the caller cannot
/// rely on reaching its own cleanup: `AcpRunTurn`'s steered path *drops* the
/// prompt future when a cancelled turn outruns its grace window, so a manual
/// deregistration at the end of `prompt` would never run for exactly the
/// turns most likely to keep emitting updates afterwards — leaking an
/// observer that then republishes a dead turn's frames onto a live console.
struct LiveTurn {
    live: Arc<StdMutex<HashMap<String, AcpObserver>>>,
    session_id: String,
}

impl LiveTurn {
    fn register(
        live: &Arc<StdMutex<HashMap<String, AcpObserver>>>,
        session_id: &str,
        observer: Option<&AcpObserver>,
    ) -> Option<Self> {
        let observer = observer?;
        live.lock()
            .ok()?
            .insert(session_id.to_string(), Arc::clone(observer));
        Some(Self {
            live: Arc::clone(live),
            session_id: session_id.to_string(),
        })
    }
}

impl Drop for LiveTurn {
    fn drop(&mut self) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(&self.session_id);
        }
    }
}

impl LocalAcpAgent {
    /// `agent` is one of `ACP_AGENTS` (the manifest already validated this).
    /// `model`, when set, is forwarded via that agent's own startup lever
    /// when this build knows one. `agent_models` is this harness's own
    /// per-agent overrides — see [`Self::agent_models`].
    pub fn new(
        agent: &str,
        model: Option<&str>,
        agent_models: HashMap<String, String>,
        workspace_root: PathBuf,
    ) -> Result<Self> {
        let def = HARNESSES.iter().find(|h| h.id == agent).ok_or_else(|| {
            OpenCompanyError::Config(format!("no local ACP harness definition for `{agent}`"))
        })?;

        let mut env = Vec::new();
        if let (Some(model), Some(var)) = (model, model_env_var(agent)) {
            env.push((var.to_string(), model.to_string()));
        }

        Ok(Self {
            harness: def,
            args: def.args.iter().map(|a| a.to_string()).collect(),
            env,
            model: model.map(str::to_string),
            agent_models,
            workspace_root,
            client: AsyncMutex::new(None),
            sessions: AsyncMutex::new(HashMap::new()),
            session_locks: AsyncMutex::new(HashMap::new()),
            pending_updates: Arc::new(StdMutex::new(HashMap::new())),
            live: Arc::new(StdMutex::new(HashMap::new())),
            load_session: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The spawned client, spawning it on first call.
    async fn client(&self) -> Result<Arc<AcpClient>> {
        let mut guard = self.client.lock().await;
        if let Some(client) = guard.as_ref() {
            return Ok(client.clone());
        }

        std::fs::create_dir_all(&self.workspace_root).map_err(|error| {
            OpenCompanyError::Config(format!(
                "could not create ACP workspace root {}: {error}",
                self.workspace_root.display()
            ))
        })?;
        let confinement = Confinement::new(&self.workspace_root)
            .map_err(|error| OpenCompanyError::Config(format!("acp workspace: {error}")))?;
        // Auto-approves permission requests by kind — see the module docs.
        let handler: Arc<dyn ClientHandler> = Arc::new(AutoApprovingFiles::new(
            ConfinedFiles::new(confinement, None),
        ));

        let pending = Arc::clone(&self.pending_updates);
        let live = Arc::clone(&self.live);
        let sink: crate::acp::client::UpdateSink = Arc::new(move |update: Value| {
            let session_id = update["sessionId"].as_str().unwrap_or_default().to_string();
            // Tee'd to whoever is watching this session's turn *before* the
            // buffer push, so the console sees a tool call at the moment the
            // adapter reports it rather than when the turn ends. The buffer
            // still gets every update: the fold reads it, not this, so the
            // live view cannot drift from the durable timeline (and a turn
            // nobody is watching does no extra work at all).
            if let Some(observer) = live
                .lock()
                .ok()
                .and_then(|live| live.get(&session_id).cloned())
                && let Some(parsed) = parse_update(&update)
            {
                observer(&parsed);
            }
            pending
                .lock()
                .unwrap()
                .entry(session_id)
                .or_default()
                .push(update);
        });

        // Resolved here, not held: an install that happened since this
        // company was built is picked up on the next turn rather than the next
        // restart. Falls back to the catalogue name so the spawn failure is
        // `NotOnPath` — which is what produces install advice — rather than a
        // path that was never there.
        let command = crate::acp::discovery::resolve_adapter(self.harness)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| self.harness.command.to_string());

        let args: Vec<&str> = self.args.iter().map(String::as_str).collect();
        let env: Vec<(&str, &str)> = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let client = AcpClient::spawn(&command, &args, &self.workspace_root, &env, handler, sink)
            .await
            .map_err(|error| {
                OpenCompanyError::Config(format!("could not start `{command}`: {error}"))
            })?;
        let hello = client
            .initialize()
            .await
            .map_err(|error| OpenCompanyError::Config(format!("acp initialize: {error}")))?;
        // The one moment the adapter says whether it can resume a session.
        // Both catalogue adapters answer `true` today (confirmed live against
        // `claude-agent-acp` 0.70.0 and `codex-acp` 1.6.2), but it is read
        // rather than assumed: an adapter that cannot must get a fresh
        // `session/new`, not a `session/load` that fails every cold start.
        let load_session = hello["agentCapabilities"]["loadSession"]
            .as_bool()
            .unwrap_or(false);
        self.load_session
            .store(load_session, std::sync::atomic::Ordering::Release);

        let client = Arc::new(client);
        *guard = Some(client.clone());
        Ok(client)
    }

    /// The per-(company, agent) session directory, created if it does not
    /// exist yet — mirrors `harness::built_in::build::agent_workspace`.
    fn session_root(&self, company: &CompanyId, agent_id: &str) -> Result<PathBuf> {
        let dir = self
            .workspace_root
            .join(company.as_ref())
            .join(agent_id)
            .join("workspace");
        std::fs::create_dir_all(&dir).map_err(|error| {
            OpenCompanyError::Config(format!(
                "could not create ACP session workspace {}: {error}",
                dir.display()
            ))
        })?;
        Ok(dir)
    }

    /// Where this (company, agent) pair's resumable session id is remembered.
    ///
    /// Beside the agent's workspace, deliberately **not** inside it: `workspace/`
    /// is the `cwd` handed to the adapter, so a file dropped in there is one
    /// the teammate can list, read and edit — bookkeeping masquerading as its
    /// project.
    fn session_record_path(&self, company: &CompanyId, agent_id: &str) -> PathBuf {
        self.workspace_root
            .join(company.as_ref())
            .join(agent_id)
            .join("acp-session.json")
    }

    /// The remembered session for this pair, if there is a usable one.
    ///
    /// A record naming a *different* harness is ignored rather than tried: a
    /// `claude-agent-acp` session id means nothing to `codex-acp`, and the
    /// record outlives a teammate being rebound from one to the other.
    fn read_session_record(&self, company: &CompanyId, agent_id: &str) -> Option<SessionRecord> {
        let raw = std::fs::read_to_string(self.session_record_path(company, agent_id)).ok()?;
        let record: SessionRecord = serde_json::from_str(&raw).ok()?;
        (record.harness == self.harness.id).then_some(record)
    }

    /// The model this teammate should be on, whatever mechanism delivers it —
    /// its own override if it has one, else the harness's default, else the
    /// adapter's.
    ///
    /// Deliberately blind to `self.env`, unlike the steering decision in
    /// [`Self::session_for`]: this answers "what should be true of this
    /// teammate", not "who is responsible for making it true". The env var is
    /// one delivery mechanism among two, and a session that got its model
    /// that way is on the same model as one that got it by
    /// `session/set_config_option`.
    fn desired_model(&self, agent_id: &str) -> Option<String> {
        self.agent_models
            .get(agent_id)
            .cloned()
            .or_else(|| self.model.clone())
    }

    /// Remembers `session_id` so the next process can resume this conversation.
    ///
    /// Best-effort: a session that cannot be written down still runs this
    /// turn, and the only cost of losing the record is the next cold start
    /// beginning a fresh conversation — which is exactly what happened before
    /// any of this existed. Failing the turn over it would trade a working
    /// agent for a bookkeeping error.
    fn write_session_record(
        &self,
        company: &CompanyId,
        agent_id: &str,
        session_id: &str,
        model: Option<&str>,
    ) {
        let record = SessionRecord {
            harness: self.harness.id.to_string(),
            session_id: session_id.to_string(),
            model: model.map(str::to_string),
        };
        let path = self.session_record_path(company, agent_id);
        // Written to a same-directory temp file and renamed over the record,
        // never written in place: a kill or a full disk mid-`fs::write` can
        // truncate the file to empty or partial JSON, which `read_session_record`
        // then silently treats as absent — losing the only pointer to the
        // conversation this record exists to preserve. A rename is atomic on
        // the platforms this ships for, so a crash lands on either the old
        // record or the new one, never a half-written one (CodeRabbit, PR
        // #1904 review).
        let tmp_path = path.with_extension("json.tmp");
        let written = serde_json::to_string(&record)
            .map_err(|error| error.to_string())
            .and_then(|json| std::fs::write(&tmp_path, json).map_err(|error| error.to_string()))
            .and_then(|()| std::fs::rename(&tmp_path, &path).map_err(|error| error.to_string()));
        if let Err(error) = written {
            tracing::warn!(
                path = %path.display(),
                %error,
                "[acp] could not remember the session; the next start will begin a fresh conversation"
            );
        }
    }

    /// Re-opens the conversation this pair had last time, if it can.
    ///
    /// The gap this closes: the `sessions` map below lives in memory, and a
    /// fresh [`LocalAcpAgent`] is built on every runtime rebuild — a manifest
    /// edit, an inference-settings change, a restart of the app. Every one of
    /// those silently started the teammate's conversation over, with no
    /// operator-visible sign that it had. ACP's own answer is `session/load`,
    /// which replays the conversation into the adapter (confirmed live: a
    /// codeword planted before a full process restart comes back after it).
    ///
    /// `None` on every failure, never an error: not supported, nothing
    /// remembered, or the adapter no longer holds that session. All of them
    /// mean the same thing to the caller — open a new one — and none is a
    /// reason to fail an operator's turn.
    ///
    /// Matched on *nothing*, deliberately. The two adapters refuse the same
    /// situation with different codes — `claude-agent-acp` with
    /// `Resource not found` (`-32002`), `codex-acp` with `Internal error`
    /// (`-32603`, `"no rollout found for thread id …"`) — so a fallback
    /// keyed on either one would hard-fail a turn on the other. Both also
    /// refuse a session that was minted and never prompted, which is why the
    /// record written at `session/new` may cost one refused round trip before
    /// the teammate's first turn ever completes.
    async fn resume_session(
        &self,
        client: &AcpClient,
        company: &CompanyId,
        agent_id: &str,
        root: &Path,
        desired_model: Option<&str>,
    ) -> Option<(String, Value)> {
        if !self.load_session.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let record = self.read_session_record(company, agent_id)?;

        // A session whose model no longer matches, and which cannot be
        // corrected, must not be resumed (PR #1904 review).
        //
        // `session/load` restores the session's own model config, and the
        // only lever this side has for changing it is
        // `session/set_config_option` — which needs a value to set. When an
        // admin removes a teammate's override from a harness that declares no
        // model of its own, the wanted state is "back to the adapter's
        // default", and there is no value that expresses it. Resuming would
        // silently keep the model the operator just deleted.
        //
        // So the conversation is given up instead, and only in that exact
        // case: a recorded model, nothing to replace it with. Losing the
        // history of a teammate whose model just changed is the honest cost —
        // and a smaller surprise than a teammate that answers on a model the
        // company no longer lists.
        if desired_model.is_none() && record.model.is_some() {
            tracing::info!(
                company = %company.as_ref(),
                agent = %agent_id,
                was = %record.model.unwrap_or_default(),
                "[acp] the model override was removed and no default replaces it; \
                 starting a fresh session rather than resuming on the old model"
            );
            return None;
        }

        let session_id = record.session_id;

        // The replay this triggers arrives as ordinary `session/update`
        // notifications on the shared sink. Two things keep it out of the
        // turn: no observer is registered yet (`prompt` registers one only
        // after this returns), and `prompt` clears this session's buffer
        // before the first `session/prompt`.
        match client
            .call(
                "session/load",
                serde_json::json!({
                    "sessionId": session_id,
                    "cwd": root.display().to_string(),
                    "mcpServers": [],
                }),
            )
            .await
        {
            Ok(loaded) => {
                tracing::info!(
                    company = %company.as_ref(),
                    agent = %agent_id,
                    "[acp] resumed the previous conversation"
                );
                Some((session_id, loaded))
            }
            Err(error) => {
                // The record is deliberately **kept**. A failed load does not
                // establish that the session is gone: `AcpClient::call`
                // answers `Gone` for an adapter that exited and `Io` for a
                // failed stdio write, and neither is the adapter saying it
                // has no such session. Dropping the record on those would
                // throw away the only pointer to a conversation that is still
                // there — and the `session/new` below is about to fail
                // against the same dead client, so the next start would find
                // nothing to resume and begin fresh for good.
                //
                // Keeping it costs nothing in the definitive case either:
                // `session/new` overwrites the record the moment it succeeds,
                // so a genuinely dead session is replaced rather than retried
                // forever. The bound on the wasted work is one refused load
                // per cold start, and only while every `session/new` is also
                // failing.
                tracing::info!(
                    company = %company.as_ref(),
                    agent = %agent_id,
                    %error,
                    "[acp] could not resume the previous conversation; starting a fresh one"
                );
                None
            }
        }
    }

    /// This session's cached ACP `sessionId`, resuming or opening one if none
    /// exists yet.
    ///
    /// Model steering happens on whichever session this ends up with — when no
    /// startup env var carries it ([`model_env_var`] returned `None` for this
    /// agent), or when `agent_id` carries its own override in
    /// [`Self::agent_models`]: the `session/new` (or `session/load`) response
    /// is inspected for a `configOptions` entry with `category: "model"` whose
    /// options include the desired value, and if found,
    /// `session/set_config_option` applies it before this session is used for
    /// anything. Confirmed live to be per-session state (not global), which is
    /// exactly the granularity wanted — a session opened here is one (company,
    /// agent) pair for its whole life.
    async fn session_for(
        &self,
        client: &AcpClient,
        company: &CompanyId,
        session_key: &str,
        agent_id: &str,
        root: &Path,
    ) -> Result<String> {
        let mut sessions = self.sessions.lock().await;
        if let Some(id) = sessions.get(session_key) {
            return Ok(id.clone());
        }

        // Resume before opening: this is the first turn *this process* runs
        // for the teammate, which is not the same thing as the first turn the
        // teammate has ever run.
        // What this teammate should be on, decided once and used for three
        // things: whether a remembered session is still resumable, what to
        // steer the session to, and what to write down about it.
        let desired = self.desired_model(agent_id);

        let resumed = self
            .resume_session(client, company, agent_id, root, desired.as_deref())
            .await;
        let was_resumed = resumed.is_some();
        let (id, raw) = match resumed {
            Some(resumed) => resumed,
            None => {
                let raw = client
                    .call(
                        "session/new",
                        serde_json::json!({ "cwd": root.display().to_string(), "mcpServers": [] }),
                    )
                    .await
                    .map_err(|error| {
                        OpenCompanyError::Config(format!("acp session/new: {error}"))
                    })?;
                let id = raw["sessionId"]
                    .as_str()
                    .ok_or_else(|| {
                        OpenCompanyError::Config(
                            "acp session/new returned no sessionId".to_string(),
                        )
                    })?
                    .to_string();
                (id, raw)
            }
        };

        // A per-agent override always takes the `session/set_config_option`
        // path, whether or not `self.env` already carries the harness's own
        // default: the env var is process-wide, set once at spawn, and two
        // agents on this harness share that one subprocess — it cannot
        // represent "this agent, specifically, on a different model" no
        // matter which model the harness itself defaults to.
        //
        // Absent an override, `self.env` carries the model only when `new()`
        // found a known env var for this agent — non-empty means the spawn
        // already handled it, so the fallback must not also fire (redundant
        // at best, and this session's model would otherwise be decided by
        // whichever of the two APIs the adapter honors last). No matching
        // `config_id` falls through the same way: either an env var already
        // carried it at spawn, or this build has no lever for this agent at
        // all (issue #1245's documented codex gap, before this fallback
        // existed) — either way, silently doing nothing here is correct, not
        // a missed error.
        //
        // A *resumed* session takes the same pass, against `session/load`'s
        // own response — but it may NOT take the `self.env` short-circuit
        // (PR #1904 review). The env var configures a subprocess at spawn,
        // which is how a *new* session arrives on the harness's model without
        // an explicit call; it cannot reach back into a session some earlier
        // process already created and configured.
        //
        // The case that breaks: a teammate had a per-agent override, an
        // admin removes it, the roster rebuilds. `self.env` is still non-empty
        // (it carries the harness default for `claude`), so the short-circuit
        // yields `None`, nothing is sent — and the resumed session keeps the
        // session-scoped override the operator just deleted. Confirmed live
        // that `session/load` restores that config: its response carries the
        // same `configOptions` model entry, `currentValue` and all.
        //
        // So a resumed session always falls back to the harness's own model,
        // and only a *fresh* one trusts the spawn to have handled it.
        //
        // So a resumed session always applies `desired`, and only a *fresh*
        // one trusts the spawn to have handled it.
        // Two cases need the model sent explicitly and they collapse to one
        // condition: a **resumed** session (an earlier process configured it,
        // and this process's env cannot reach back into it), and a **fresh**
        // session on a harness whose spawn exported no model var at all. In
        // both, nothing else has put this session on the right model.
        //
        // The remaining case — a fresh session on a harness that did export
        // one — is already on the harness default, so only a per-agent
        // override still needs sending.
        let to_apply = if was_resumed || self.env.is_empty() {
            desired.clone()
        } else {
            self.agent_models.get(agent_id).cloned()
        };
        // Track the model that is actually in effect. On a fresh session,
        // `self.model` was delivered through the harness startup environment
        // when it is non-empty, even though there is no ACP config option to
        // apply here. Preserve that fact so a later restart can detect that a
        // removed harness model must not be restored by `session/load`.
        // Never record a desired value that was unavailable to ACP, however.
        //
        // A resumed session starts from what its own record already says,
        // not `None`: `session/load` restores that session-scoped config
        // regardless of what `to_apply` ends up being below, so a resume
        // that sends nothing (no matching config option for `desired`) is
        // still on its recorded model, not the adapter default. Seeding
        // `None` here would overwrite a true record with a false one and
        // defeat the very guard at `resume_session`'s `desired_model.is_none()
        // && record.model.is_some()` check on the next restart (CodeRabbit,
        // PR #1904 review).
        let mut applied_model: Option<String> = if was_resumed {
            self.read_session_record(company, agent_id)
                .and_then(|record| record.model)
        } else {
            (!self.env.is_empty()).then(|| self.model.clone()).flatten()
        };
        if let Some(model) = to_apply.as_deref()
            && let Some(config_id) = model_config_id(&raw, model)
        {
            client
                .call(
                    "session/set_config_option",
                    serde_json::json!({
                        "sessionId": id,
                        "configId": config_id,
                        "value": model,
                    }),
                )
                .await
                .map_err(|error| {
                    OpenCompanyError::Config(format!(
                        "acp session/set_config_option (model `{model}`): {error}"
                    ))
                })?;
            applied_model = Some(model.to_string());
        }

        // Written for both paths, and after the steering above rather than
        // beside `session/new`: the record's job is to say which session this
        // teammate has *and what model it is on*, and the second half is only
        // settled here. A resumed session whose model was just corrected
        // rewrites its record with the new value, so the next start does not
        // see a mismatch that no longer exists. Only the *applied* model is
        // recorded, not a desired value that the session's config options do
        // not offer — a false record would mislead the next start into
        // thinking the model was applied when the session is on its default.
        self.write_session_record(company, agent_id, &id, applied_model.as_deref());

        sessions.insert(session_key.to_string(), id.clone());
        Ok(id)
    }

    /// This session's turn lock, created on first use.
    ///
    /// Keyed by ACP `sessionId` rather than by `session_key` because the
    /// session is what actually gets corrupted by two concurrent prompts: it
    /// is one conversation, one update stream, and one entry in both the
    /// pending-update buffer and the live-observer map.
    async fn session_lock(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.session_locks.lock().await;
        Arc::clone(
            locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    /// `session_key` is `"{company}::{agent_id}"` — recovers `agent_id` by
    /// stripping the company prefix, since `AcpAgent::prompt` does not carry
    /// it separately. Agent ids are `snake_case` (manifest-validated) and
    /// cannot themselves contain `::`, so this split is unambiguous.
    fn agent_id_of<'a>(company: &CompanyId, session_key: &'a str) -> &'a str {
        session_key
            .strip_prefix(company.as_ref())
            .and_then(|rest| rest.strip_prefix("::"))
            .unwrap_or(session_key)
    }
}

/// Finds the `configId` to set to reach `desired_model`, from a fresh
/// `session/new` response's `configOptions` — the entry whose `category` is
/// `"model"` and whose `options` include a `value` matching `desired_model`.
/// `None` when nothing matches: either this adapter advertises no such
/// option, or it does but not for this exact value.
///
/// Accepts both `configId` (the ACP spec's own name) and `id` — confirmed
/// live that `codex-acp` emits `id`, matching the same quirk documented for
/// `claude-agent-acp` in `harness::acp::run_turn`.
///
/// `pub` (not private) so `tests/acp_live_smoke.rs` can pin this parsing
/// against a captured real response without a live spawn — the one part of
/// the fallback that can be tested deterministically and in CI.
pub fn model_config_id(session_new_result: &Value, desired_model: &str) -> Option<String> {
    session_new_result["configOptions"]
        .as_array()?
        .iter()
        .find_map(|opt| {
            if opt.get("category").and_then(|c| c.as_str()) != Some("model") {
                return None;
            }
            let matches = opt
                .get("options")
                .and_then(|o| o.as_array())
                .is_some_and(|options| {
                    options
                        .iter()
                        .any(|o| o.get("value").and_then(|v| v.as_str()) == Some(desired_model))
                });
            if !matches {
                return None;
            }
            opt.get("configId")
                .or_else(|| opt.get("id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

/// Translates one raw `session/update` notification into this crate's
/// [`AcpUpdate`], or `None` for a kind that is dropped rather than
/// approximated (`plan`, `available_commands_update`, …) — see
/// `harness::acp::run_turn`'s own module docs for the mapping table this
/// mirrors.
fn parse_update(raw: &Value) -> Option<AcpUpdate> {
    let update = raw.get("update")?;
    match update.get("sessionUpdate")?.as_str()? {
        "agent_message_chunk" => Some(AcpUpdate::MessageChunk(
            update["content"]["text"].as_str()?.to_string(),
        )),
        "agent_thought_chunk" => Some(AcpUpdate::ThoughtChunk),
        "tool_call" => Some(AcpUpdate::ToolCall {
            id: update["toolCallId"].as_str()?.to_string(),
            title: bounded(
                update["title"].as_str().unwrap_or_default(),
                MAX_TITLE_CHARS,
            ),
        }),
        "tool_call_update" => Some(AcpUpdate::ToolCallUpdate {
            id: update["toolCallId"].as_str()?.to_string(),
            status: update["status"].as_str().unwrap_or_default().to_string(),
            result: update
                .get("content")
                .and_then(|c| c.as_array())
                .map(|blocks| {
                    let joined = blocks
                        .iter()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("");
                    bounded(&joined, MAX_RESULT_CHARS)
                }),
        }),
        // `user_message_chunk` lands here, and dropping it is the point: it
        // arrives only in `session/load`'s replay of a resumed conversation
        // (the operator's own earlier messages), never during a turn. Mapping
        // it to anything would put a past message on this turn's timeline.
        _ => None,
    }
}

#[async_trait]
impl AcpAgent for LocalAcpAgent {
    async fn prompt(
        &self,
        company: &CompanyId,
        session_key: &str,
        message: &str,
        observer: Option<&AcpObserver>,
    ) -> Result<AcpTurn> {
        let client = self.client().await?;
        let agent_id = Self::agent_id_of(company, session_key);
        let root = self.session_root(company, agent_id)?;
        let session_id = self
            .session_for(&client, company, session_key, agent_id, &root)
            .await?;

        // One turn at a time on this session — see the module docs. Taken
        // before the buffer is cleared and held past the drain, so a queued
        // turn cannot clear a running turn's updates out from under it, or
        // displace its observer.
        let turn_lock = self.session_lock(&session_id).await;
        let _turn = turn_lock.lock().await;

        // Clear any stale buffer before the turn starts, so the drain below
        // sees exactly this turn's updates and nothing left over from one
        // that timed out or was cancelled without being read — or from a
        // `session/load` replay, which lands here as ordinary updates.
        self.pending_updates.lock().unwrap().remove(&session_id);

        // Registered only now, after the clear: everything from here to the
        // guard's drop is this turn, and nothing before it was. Dropped when
        // this function returns *or* when its future is dropped mid-turn.
        let _live = LiveTurn::register(&self.live, &session_id, observer);

        let stop_reason = client
            .prompt(&session_id, message)
            .await
            .map_err(|error| OpenCompanyError::Config(format!("acp prompt: {error}")))?;

        let raw = self
            .pending_updates
            .lock()
            .unwrap()
            .remove(&session_id)
            .unwrap_or_default();
        let updates = raw.iter().filter_map(parse_update).collect();
        Ok(AcpTurn {
            updates,
            stop_reason,
        })
    }

    async fn cancel(&self, company: &CompanyId, session_key: &str) -> Result<()> {
        let session_id = {
            let sessions = self.sessions.lock().await;
            sessions.get(session_key).cloned()
        };
        let Some(session_id) = session_id else {
            // No session ever opened for this (company, agent) — nothing to
            // cancel, and asking a client that may not exist yet would spawn
            // one just to tell it to stop.
            return Ok(());
        };
        let client = { self.client.lock().await.clone() };
        let Some(client) = client else {
            return Ok(());
        };
        let _ = company; // carried for symmetry with `prompt`; not needed here
        client
            .cancel(&session_id)
            .await
            .map_err(|error| OpenCompanyError::Config(format!("acp cancel: {error}")))
    }
}

/// Builds a fresh [`LocalAcpAgent`] per call — no caching, matching
/// `harness::lanes::built_in_lane`'s own precedent of building a fresh pool
/// on every `RuntimeBuilder::build`. A rebuild is rare (a manifest or
/// inference-settings change), and the old agent's subprocess is killed on
/// drop (`AcpClient::kill_on_drop`), so nothing leaks.
pub struct LocalAcpAgentFactory;

impl AcpAgentFactory for LocalAcpAgentFactory {
    fn build(
        &self,
        agent: &str,
        model: Option<&str>,
        agent_models: &HashMap<String, String>,
        workspace_root: &Path,
    ) -> Result<Arc<dyn AcpAgent>> {
        Ok(Arc::new(LocalAcpAgent::new(
            agent,
            model,
            agent_models.clone(),
            workspace_root.to_path_buf(),
        )?))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn agent(root: &Path) -> LocalAcpAgent {
        LocalAcpAgent::new("claude", None, HashMap::new(), root.to_path_buf())
            .expect("`claude` is a catalogue harness")
    }

    #[test]
    fn a_remembered_session_comes_back_for_the_same_pair() {
        // The whole point: the in-memory `sessions` map dies with the process,
        // and every runtime rebuild builds a fresh agent. Without a record on
        // disk, a teammate's conversation silently started over on a restart —
        // and nothing on the operator's screen said so.
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path());
        let acme = CompanyId::new("acme");
        std::fs::create_dir_all(dir.path().join("acme").join("ceo")).unwrap();

        assert!(agent.read_session_record(&acme, "ceo").is_none());
        agent.write_session_record(&acme, "ceo", "sess-1", None);
        assert_eq!(
            agent
                .read_session_record(&acme, "ceo")
                .map(|r| r.session_id),
            Some("sess-1".to_string())
        );

        // Per pair, never shared: two teammates resuming one conversation is
        // the same defect as two desks sharing a session key.
        assert!(agent.read_session_record(&acme, "cto").is_none());
        assert!(
            agent
                .read_session_record(&CompanyId::new("globex"), "ceo")
                .is_none()
        );
    }

    #[test]
    fn a_record_from_another_harness_is_not_offered() {
        // A `claude-agent-acp` session id means nothing to `codex-acp`, and
        // the record outlives a teammate being rebound between them. Loading
        // it would spend a round trip to be told "resource not found".
        let dir = tempfile::tempdir().unwrap();
        let acme = CompanyId::new("acme");
        std::fs::create_dir_all(dir.path().join("acme").join("ceo")).unwrap();

        agent(dir.path()).write_session_record(&acme, "ceo", "sess-1", None);

        let codex = LocalAcpAgent::new("codex", None, HashMap::new(), dir.path().to_path_buf())
            .expect("`codex` is a catalogue harness");
        assert!(codex.read_session_record(&acme, "ceo").is_none());
    }

    #[test]
    fn a_new_session_replaces_the_one_that_would_not_load() {
        // Why a failed `session/load` does NOT delete the record (PR #1904
        // review): a load can fail because the adapter *exited*, which says
        // nothing about whether the session still exists — and the
        // `session/new` that follows is about to fail against the same dead
        // client. Deleting there would throw away the only pointer to a
        // recoverable conversation.
        //
        // What makes keeping it safe is this: a session that really is gone
        // gets overwritten the moment a replacement is opened, so the stale
        // id cannot be retried forever.
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path());
        let acme = CompanyId::new("acme");
        std::fs::create_dir_all(dir.path().join("acme").join("ceo")).unwrap();

        agent.write_session_record(&acme, "ceo", "sess-dead", None);
        agent.write_session_record(&acme, "ceo", "sess-new", None);
        assert_eq!(
            agent
                .read_session_record(&acme, "ceo")
                .map(|r| r.session_id),
            Some("sess-new".to_string()),
            "the replacement is what the next start resumes"
        );
    }

    #[test]
    fn a_record_remembers_which_model_its_session_is_on() {
        // Why the model is recorded at all (PR #1904 review): a resumed
        // session restores its own *session-scoped* model config, so without
        // this there is no way to notice the config no longer matches what
        // the company asks for.
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path());
        let acme = CompanyId::new("acme");
        std::fs::create_dir_all(dir.path().join("acme").join("ceo")).unwrap();

        agent.write_session_record(&acme, "ceo", "sess-1", Some("claude-opus-4-6"));
        let record = agent.read_session_record(&acme, "ceo").expect("written");
        assert_eq!(record.model.as_deref(), Some("claude-opus-4-6"));

        // A record written before this field existed reads as "no model ever
        // chosen" — the same value a never-configured session has, so an old
        // record resumes exactly as it did.
        std::fs::write(
            agent.session_record_path(&acme, "ceo"),
            r#"{"harness":"claude","sessionId":"sess-old"}"#,
        )
        .unwrap();
        let legacy = agent.read_session_record(&acme, "ceo").expect("still read");
        assert_eq!(legacy.session_id, "sess-old");
        assert_eq!(legacy.model, None);
    }

    #[test]
    fn the_desired_model_is_the_override_then_the_harness_default() {
        // Deliberately blind to `self.env`: this answers what should be true
        // of the teammate, not who delivers it. A session that got its model
        // from the spawn env is on the same model as one that got it from
        // `session/set_config_option`, and the record must say so either way.
        let dir = tempfile::tempdir().unwrap();
        let mut overrides = HashMap::new();
        overrides.insert("cto".to_string(), "claude-haiku-4-5".to_string());
        let configured = LocalAcpAgent::new(
            "claude",
            Some("claude-sonnet-4-5"),
            overrides,
            dir.path().to_path_buf(),
        )
        .expect("`claude` is a catalogue harness");

        assert_eq!(
            configured.desired_model("cto").as_deref(),
            Some("claude-haiku-4-5"),
            "a teammate's own override wins"
        );
        assert_eq!(
            configured.desired_model("ceo").as_deref(),
            Some("claude-sonnet-4-5"),
            "and the harness default covers everyone else — even though the \
             spawn env is what actually carries it"
        );

        // Nothing declared anywhere: the adapter's own default, which no
        // `session/set_config_option` can name.
        let bare = agent(dir.path());
        assert_eq!(bare.desired_model("ceo"), None);
    }

    #[test]
    fn the_record_lives_outside_the_agents_own_workspace() {
        // `workspace/` is the `cwd` handed to the adapter. A file written in
        // there is one the teammate can list, read and edit — and one it could
        // "tidy up" mid-session.
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path());
        let acme = CompanyId::new("acme");

        let record = agent.session_record_path(&acme, "ceo");
        let cwd = agent.session_root(&acme, "ceo").unwrap();
        assert!(
            !record.starts_with(&cwd),
            "{} must not sit inside {}",
            record.display(),
            cwd.display()
        );
    }

    #[test]
    fn an_unwritable_record_does_not_fail_the_turn() {
        // Bookkeeping must never cost an operator a working agent: the worst
        // a lost record can do is make the next cold start begin fresh, which
        // is what every start did before this existed.
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path());
        // No `acme/ceo/` directory, so the write has nowhere to land.
        agent.write_session_record(&CompanyId::new("acme"), "ceo", "sess-1", None);
        assert!(
            agent
                .read_session_record(&CompanyId::new("acme"), "ceo")
                .is_none()
        );
    }

    #[test]
    fn wire_text_is_bounded_before_it_reaches_the_timeline() {
        // A tool call's `title` and result are unvalidated text from an
        // external process, and they now travel to two places at once: the
        // durable step and every watching console.
        let long = "x".repeat(MAX_TITLE_CHARS * 2);
        let update = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "c1",
                "title": long,
            }
        });
        let Some(AcpUpdate::ToolCall { title, .. }) = parse_update(&update) else {
            panic!("a tool call parses");
        };
        assert_eq!(
            title.chars().count(),
            MAX_TITLE_CHARS + 1,
            "bounded, plus the ellipsis"
        );

        // Multi-byte input must be cut on a character, never a byte — a byte
        // slice through a UTF-8 sequence panics.
        assert_eq!(bounded("héllo wörld", 4), "héll…");
        assert_eq!(
            bounded("short", 99),
            "short",
            "an unbounded string is untouched"
        );
    }

    #[test]
    fn a_replayed_user_message_is_not_this_turns_execution_state() {
        // `session/load` replays the resumed conversation as ordinary
        // `session/update` notifications, including the operator's own past
        // messages. Mapping one would put a months-old message on this turn's
        // timeline.
        let update = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "user_message_chunk",
                "content": { "type": "text", "text": "what did I say before?" }
            }
        });
        assert!(parse_update(&update).is_none());
    }

    #[test]
    fn an_observer_is_registered_for_one_turn_and_no_longer() {
        // The guard, not a matching pair of calls: the steered path *drops*
        // the prompt future for a turn that ignored its cancel, and a manual
        // deregistration would never run for exactly those turns.
        let live: Arc<StdMutex<HashMap<String, AcpObserver>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let seen = Arc::new(StdMutex::new(0usize));
        let counter = Arc::clone(&seen);
        let observer: AcpObserver = Arc::new(move |_| {
            *counter.lock().unwrap() += 1;
        });

        {
            let _guard = LiveTurn::register(&live, "s1", Some(&observer));
            assert!(live.lock().unwrap().contains_key("s1"));
            live.lock().unwrap()["s1"](&AcpUpdate::ThoughtChunk);
        }
        assert!(
            live.lock().unwrap().is_empty(),
            "the turn's observer goes when the turn does"
        );
        assert_eq!(*seen.lock().unwrap(), 1);

        // A turn nobody is watching registers nothing at all, so the sink does
        // no per-update work for it.
        assert!(LiveTurn::register(&live, "s1", None).is_none());
        assert!(live.lock().unwrap().is_empty());
    }
}
