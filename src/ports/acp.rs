//! The [`AcpAgent`]/[`AcpAgentFactory`] ports: running a company's turn on an
//! external agent over the Agent Client Protocol, instead of the embedded
//! OpenHuman harness (issue #1245).
//!
//! ## Why these live here, not under `harness`
//!
//! Everything under `crate::harness` is gated behind the `openhuman` feature
//! — the embedded engine and its dependency tree. The desktop shell, which is
//! the whole reason ACP exists (an operator's own coding CLI, no credential
//! from us at all), deliberately does **not** enable that feature: pulling in
//! the entire vendored OpenHuman runtime just to reach two trait definitions
//! would be exactly backwards for "the operator's own subscription, nothing
//! to configure."
//!
//! So the port — what an ACP agent *is*, and what builds one — lives here,
//! ungated, alongside every other cross-cutting port
//! ([`SecretStore`](crate::ports::SecretStore),
//! [`ContextStore`](crate::ports::ContextStore), …). `crate::harness::acp`
//! (openhuman-gated) re-exports these and adds
//! [`AcpRunTurn`](crate::harness::acp::run_turn::AcpRunTurn) — the
//! `RunTurn` adapter that folds an [`AcpTurn`] into the embedded engine's own
//! [`TurnStep`](crate::ports::types::TurnStep) shape — because *that* piece
//! genuinely needs the embedded engine's types.
//!
//! ## What this unlocks
//!
//! - **A desktop company with no key.** The embedded host runs a turn on the
//!   operator's own `claude-agent-acp`, against their existing subscription.
//! - **Reverse dispatch.** A cloud host hands a task to a runner on someone's
//!   machine; the runner is an ACP agent as far as this is concerned.
//! - **Any other harness.** Codex, and anything else that speaks ACP.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::CompanyId;

/// One `session/update` payload, already parsed into what this layer needs.
///
/// A narrow enum rather than raw JSON, so the wire-format knowledge stays in
/// the transport and the folding downstream stays testable without one.
#[derive(Clone, Debug, PartialEq)]
pub enum AcpUpdate {
    /// Assistant text. Concatenated, in arrival order, into the reply.
    MessageChunk(String),
    /// Reasoning. Coalesced into a single step — the console shows "Thinking",
    /// never the content, matching what the OpenHuman path surfaces.
    ThoughtChunk,
    /// A tool call started.
    ToolCall { id: String, title: String },
    /// A tool call progressed or finished.
    ToolCallUpdate {
        id: String,
        /// ACP's `pending` / `in_progress` / `completed` / `failed`.
        status: String,
        /// A short summary of what came back, already bounded by the transport.
        result: Option<String>,
    },
}

/// What an ACP agent reports for one turn.
#[derive(Clone, Debug, Default)]
pub struct AcpTurn {
    pub updates: Vec<AcpUpdate>,
    /// ACP's `stopReason`.
    pub stop_reason: String,
}

/// Watches a turn's updates **as they arrive**, before the turn returns.
///
/// The whole of an ACP turn used to be observable only after
/// `session/prompt` resolved: the transport buffered every `session/update`
/// and handed back one [`AcpTurn`] at the end. That is fine for the durable
/// timeline, which is folded from the same buffer either way, and useless for
/// the operator watching a five-minute turn — an ACP-run teammate sat silent
/// until it was finished, while a `built_in`-run one showed each tool call as
/// it started.
///
/// So the port carries an optional observer rather than a second "streaming"
/// method: the buffer is still what the fold reads (the live view and the
/// final timeline can never disagree, because one is not derived from the
/// other), and an implementation with nothing to stream — the runner lane,
/// whose own wire hands back a whole turn — simply ignores it.
///
/// Called from whatever task the transport reads its wire on, so it must not
/// block: [`crate::harness::acp::run_turn`]'s implementation publishes onto a
/// broadcast bus and returns.
pub type AcpObserver = std::sync::Arc<dyn Fn(&AcpUpdate) + Send + Sync>;

/// An ACP agent this host can run a turn on.
///
/// Implemented by the desktop (a subprocess over stdio) and, later, by the
/// runner lane (a socket). Deliberately says nothing about transport.
#[async_trait]
pub trait AcpAgent: Send + Sync {
    /// Runs one turn and returns everything it produced.
    ///
    /// `session_key` is stable for a (company, agent) pair so the agent can
    /// keep a conversation rather than starting fresh each turn.
    ///
    /// `observer`, when set, is called with each update as it arrives — see
    /// [`AcpObserver`]. Every update passed to it is also in the returned
    /// [`AcpTurn`]; observing is a tee, never a hand-off.
    async fn prompt(
        &self,
        company: &CompanyId,
        session_key: &str,
        message: &str,
        observer: Option<&AcpObserver>,
    ) -> Result<AcpTurn>;

    /// Asks the agent to stop the turn in flight.
    ///
    /// Advisory, and the caller must treat it that way: ACP's `session/cancel`
    /// is a notification, and a harness inside a long tool call notices only
    /// when that call returns.
    async fn cancel(&self, company: &CompanyId, session_key: &str) -> Result<()>;
}

/// Builds an [`AcpAgent`] for one declared `transport = "local"` harness.
///
/// A port, exactly like [`AcpAgent`] itself: only the desktop shell can
/// actually spawn a subprocess, so this crate defines the seam and the
/// desktop supplies the implementation. `lanes::build` receives this as
/// `Option<&dyn AcpAgentFactory>` — `None` on a server build, which is why a
/// `local` acp harness there still resolves to `unavailable` rather than a
/// broken or panicking build attempt.
///
/// Synchronous and infallible-to-call-lazily on purpose: building the value
/// (a struct holding the command/args/env to spawn) does no I/O — the actual
/// subprocess spawns lazily, on the agent's first `prompt` — so `lanes::build`
/// itself never blocks on process startup or a harness that is slow to boot.
pub trait AcpAgentFactory: Send + Sync {
    /// `agent` is one of `ACP_AGENTS` (the manifest already validated this).
    /// `model`, when set, is forwarded to that agent's own startup lever
    /// where this build knows one — see the implementation's own docs for
    /// which agents that currently covers. `agent_models` is the harness's
    /// own agents' per-agent overrides (issue #1245's follow-up), keyed by
    /// agent id — an agent absent from the map takes `model`, the harness's
    /// own default, unchanged. `workspace_root` is the same root the
    /// embedded engine roots a company's agent workspaces under
    /// (`HarnessDeps::workspace_root`) — the factory has no other way to
    /// learn it, since it is built once and shared across every company the
    /// host runs, not constructed fresh per company.
    fn build(
        &self,
        agent: &str,
        model: Option<&str>,
        agent_models: &std::collections::HashMap<String, String>,
        workspace_root: &std::path::Path,
    ) -> Result<Arc<dyn AcpAgent>>;
}
