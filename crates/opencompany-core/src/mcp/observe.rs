//! What an agent's MCP calls did, read from the turn's progress stream.
//!
//! Every `mcp_call_tool` call — OpenHuman's native bridge, and the registry
//! tool behind [`OcMcpRegistryScopedTool`](crate::mcp::agent::OcMcpRegistryScopedTool)
//! — attaches a [`McpCallOutcome`] to its result, which OpenHuman forwards on
//! the completed-call event. [`AgentMcpObserver::observe`] reads those after a
//! turn: a call the server answered is metered as an `OauthCall` sample, and a
//! call that failed before it was answered becomes a scrubbed [`McpFailure`]
//! on the company's [`McpCallObserver`], which the brain drains onto the
//! operator's bubble and the journal.
//!
//! Compiled only under `feature = "openhuman"`.

use std::sync::{Arc, Mutex};

use openhuman_core as oh;

use oh::agent::progress::AgentProgress;
use tinymcp::McpCallOutcome;

use crate::company::mcp::McpServerDecl;
use crate::mcp::agent::registry_outcome;
use crate::mcp::probe::{McpFailure, classify_call_error, operator_message};
use crate::ports::UsageMeter;
use crate::ports::types::CompanyId;
use crate::redact::scrub;

/// The prefix tinymcp's bridge puts in front of a failed call's error text.
const FAILED_PREFIX: &str = "mcp_call_tool failed: ";

/// The company's record of MCP calls that failed during agent turns.
///
/// Cheap to [`Clone`]: every agent built from one
/// [`HarnessDeps`](crate::harness::HarnessDeps) records into the same list,
/// so a failure in a delegated desk turn reaches the operator turn that
/// delegated it.
#[derive(Clone, Default)]
pub struct McpCallObserver {
    failures: Arc<Mutex<Vec<McpFailure>>>,
    servers: Arc<Mutex<Option<Arc<ObservedServers>>>>,
}

/// What classifying and scrubbing a failure needs of the declared servers: who
/// is configured, and every credential value to scrub. Held once per company
/// observer, not per agent.
#[derive(PartialEq, Eq)]
struct ObservedServers {
    configured: Vec<(String, bool)>,
    secrets: Vec<String>,
}

impl ObservedServers {
    fn of(decls: &[McpServerDecl]) -> Self {
        Self {
            configured: decls
                .iter()
                .map(|decl| (decl.name.clone(), decl.auth.is_configured()))
                .collect(),
            secrets: decls
                .iter()
                .flat_map(|decl| decl.auth.secret_values())
                .collect(),
        }
    }

    fn auth_configured(&self, server: &str) -> Option<bool> {
        self.configured
            .iter()
            .find(|(name, _)| name == server)
            .map(|(_, configured)| *configured)
    }
}

impl McpCallObserver {
    /// The observer one agent's turns are read with.
    pub fn for_agent(
        &self,
        company: CompanyId,
        agent: impl Into<String>,
        meter: Option<Arc<dyn UsageMeter>>,
        servers: impl AsRef<[McpServerDecl]>,
    ) -> AgentMcpObserver {
        AgentMcpObserver {
            sink: self.clone(),
            company,
            agent: agent.into(),
            meter,
            servers: self.share_servers(servers.as_ref()),
        }
    }

    fn share_servers(&self, decls: &[McpServerDecl]) -> Arc<ObservedServers> {
        let observed = ObservedServers::of(decls);
        let mut shared = self.servers.lock().expect("mcp servers");
        match shared.as_ref() {
            Some(current) if **current == observed => Arc::clone(current),
            _ => {
                let fresh = Arc::new(observed);
                *shared = Some(Arc::clone(&fresh));
                fresh
            }
        }
    }

    /// Records one failure.
    pub fn record(&self, failure: McpFailure) {
        self.failures.lock().expect("mcp failures").push(failure);
    }

    /// Forgets every recorded failure, so a prior turn's never reach this one.
    pub fn clear(&self) {
        self.failures.lock().expect("mcp failures").clear();
    }

    /// Takes every recorded failure, oldest first.
    pub fn drain(&self) -> Vec<McpFailure> {
        std::mem::take(&mut *self.failures.lock().expect("mcp failures"))
    }

    #[cfg(test)]
    fn shared_servers_of(&self, agent: &AgentMcpObserver) -> bool {
        self.servers
            .lock()
            .expect("mcp servers")
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &agent.servers))
    }

    /// How many failures are recorded.
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.failures.lock().expect("mcp failures").len()
    }
}

/// Reads one agent's completed MCP calls.
#[derive(Clone)]
pub struct AgentMcpObserver {
    sink: McpCallObserver,
    company: CompanyId,
    agent: String,
    meter: Option<Arc<dyn UsageMeter>>,
    servers: Arc<ObservedServers>,
}

impl AgentMcpObserver {
    /// An observer that meters nothing and records into a list nobody reads.
    pub fn off() -> Self {
        McpCallObserver::default().for_agent(CompanyId::new("unobserved"), "", None, [])
    }

    /// Meters the answered calls in `events` and records the failed ones,
    /// returning the failures in call order.
    pub async fn observe(&self, events: &[AgentProgress]) -> Vec<McpFailure> {
        let mut failures = Vec::new();
        for (outcome, output) in call_outcomes(events) {
            if outcome.ok {
                self.meter_answered(&outcome.server).await;
                continue;
            }
            if let Some(failure) = self.failure(&outcome, output) {
                tracing::debug!(
                    company = %self.company,
                    agent = %self.agent,
                    server = %failure.server,
                    tool = %failure.tool,
                    status = %failure.status,
                    "[mcp] call failed before the server answered"
                );
                self.sink.record(failure.clone());
                failures.push(failure);
            }
        }
        failures
    }

    async fn meter_answered(&self, server: &str) {
        if let Some(meter) = &self.meter {
            crate::metering::record_oauth_call(
                meter.as_ref(),
                &self.company,
                &self.agent,
                &crate::metering::mcp_provider(server),
                crate::ports::now_millis(),
            )
            .await;
        }
    }

    /// The scrubbed failure an unanswered call amounts to, or `None` when the
    /// outcome is a refusal rather than a failure.
    ///
    /// The server and tool are the names the caller typed, so they are
    /// scrubbed with every declared server's credentials, like the message.
    fn failure(&self, outcome: &McpCallOutcome, output: &str) -> Option<McpFailure> {
        let error = outcome.error.as_ref()?;
        let auth_configured = self.servers.auth_configured(&outcome.server);
        let secrets = &self.servers.secrets;
        let detail = registry_outcome::failure_text(output).unwrap_or_else(|| {
            output
                .strip_prefix(FAILED_PREFIX)
                .unwrap_or(output)
                .to_string()
        });
        let detail = match detail.trim() {
            "" => error
                .code
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_string(),
            trimmed => trimmed.to_string(),
        };
        let class = classify_call_error(error, &detail, auth_configured)?;
        Some(McpFailure {
            server: scrub(&outcome.server, secrets),
            tool: scrub(&outcome.tool, secrets),
            status: class.code(),
            hint: class.auth_hint.clone(),
            scrubbed_message: scrub(&operator_message(&outcome.server, &class, &detail), secrets),
        })
    }
}

/// Every MCP call outcome a turn's completed tool calls carry, with the
/// call's result text, in event order.
pub fn call_outcomes(events: &[AgentProgress]) -> Vec<(McpCallOutcome, &str)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentProgress::ToolCallCompleted {
                structured, output, ..
            }
            | AgentProgress::SubagentToolCallCompleted {
                structured, output, ..
            } => structured
                .as_ref()
                .and_then(McpCallOutcome::from_metadata)
                .map(|outcome| (outcome, output.as_str())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "observe_tests.rs"]
mod tests;
