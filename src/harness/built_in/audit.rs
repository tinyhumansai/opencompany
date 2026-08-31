//! Fail-closed shell auditing: record the command's intent **before** it runs
//! (issue #775).
//!
//! [`toolbelt::shell_audit`](crate::harness::toolbelt::shell_audit) makes the
//! audit sink host-owned and unreachable from the agent's sanctioned write
//! paths. That closes one half of the problem. This module closes the other.
//!
//! # Why an init-time gate was not enough
//!
//! The toolbelt already withholds the whole `shell` namespace when the audit
//! logger cannot be built, and the doc on
//! [`shell_tools`](crate::harness::toolbelt::shell_tools) claims that means a
//! shell can "never run commands with no audit record". That claim was only
//! true at *build* time. OpenHuman's vendored `ShellTool::emit_audit` runs
//! **after** execution and is warn-and-continue by explicit design ("audit must
//! never block or fail a tool call"), so a sink that became unwritable after the
//! agent was built yielded commands that ran with **zero** record — silently.
//!
//! That is not a nuisance failure mode. An attacker holding `shell` can fill the
//! volume on purpose, so under warn-and-continue **filling the disk mints
//! unaudited shell**. Refusing instead turns that into an outage, which is the
//! safe direction and matches what this repo already chooses elsewhere: boot
//! refuses outright on an unwritable journal root
//! (`crate::app::journal`'s `ensure_writable`).
//!
//! # What [`AuditedShellTool`] does
//!
//! One append, before delegating: an `intent` line carrying the command, on the
//! same logger (and therefore the same `O_APPEND` + `fsync` path) the vendored
//! result line uses. If that append fails the tool returns an error naming the
//! sink and the underlying I/O error, and **the command never runs**.
//!
//! The vendored post-execution result line stays warn-only, deliberately:
//! refusing *after* execution refuses nothing. A side effect worth having is
//! that a command which kills the process — or destroys the sink — still has its
//! intent on disk, fsynced, before it ran.
//!
//! # What this is not
//!
//! It is not tamper-evidence. Everything in a tenant is one uid in one process,
//! so a deliberate `rm` against the host-side sink still succeeds; hash chaining
//! in-container would be theatre, since the attacker would hold both the chain
//! and the key. What changed is that the *sanctioned* write paths refuse the
//! sink and the destroying command's intent line is already fsynced. See
//! `docs/spec/security/agent-isolation.md`.

use std::sync::Arc;

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::security::{AuditEvent, AuditEventType};
use oh::tools::{
    PermissionLevel, ShellTool, Tool, ToolCallOptions, ToolCategory, ToolResult, ToolScope,
    ToolSpec,
};
// Not re-exported from `oh::tools` (upstream's `pub use traits::{…}` list omits
// it), so it is named through the module it lives in.
use oh::tools::traits::ToolTimeout;

use crate::harness::toolbelt::ShellAudit;

/// The audit `actor.channel` an intent line carries.
///
/// Deliberately distinct from the vendored result line's `tool:shell`, so the
/// two phases of one command are separable by a reader (and by a test) without
/// parsing timestamps. The intent line also carries **no** `result` object,
/// which the vendored line always does — a second, structural discriminator.
pub const INTENT_CHANNEL: &str = "tool:shell:intent";

/// The `risk_level` recorded on an intent line.
///
/// Matches what the vendored `emit_audit` records on the result line. The
/// classification the security policy performs is a *gate* decision made further
/// in; recording a guess here would put a second, disagreeing risk opinion in
/// the same file.
const INTENT_RISK_LEVEL: &str = "unknown";

/// A [`ShellTool`] that will not run a command it could not first record.
///
/// Wraps rather than forks the vendored tool: `vendor/` is a submodule and the
/// upstream warn-and-continue result line is correct *for upstream*. Every
/// [`Tool`] method delegates, so the wrapper is indistinguishable from a bare
/// `ShellTool` to the agent, the grant gate
/// ([`namespace_of`](crate::harness::toolbelt::namespace_of) keys on
/// `name() == "shell"`) and the workflow slug lookup.
pub struct AuditedShellTool {
    /// The vendored tool. Holds its own clone of the same logger, so its
    /// post-execution result line lands in the same file, appended through the
    /// same write lock.
    inner: ShellTool,
    /// The logger plus the path it appends to. The path is only ever used to
    /// *name* the sink in a refusal.
    audit: ShellAudit,
}

impl AuditedShellTool {
    /// Wraps `inner`, appending an intent line through `audit` before every
    /// delegated call.
    ///
    /// `inner` must have been built over `audit.logger` — otherwise the intent
    /// line and the result line land in different files. [`shell_tools`] is the
    /// only construction site and does exactly that.
    ///
    /// [`shell_tools`]: crate::harness::toolbelt::shell_tools
    pub fn new(inner: ShellTool, audit: ShellAudit) -> Self {
        Self { inner, audit }
    }

    /// Append this call's intent line, or produce the refusal that replaces the
    /// command.
    ///
    /// `Some(result)` means **do not execute**. The message names the sink and
    /// the I/O error because the operator-visible symptom of this refusal is a
    /// shell that stopped working, and "which file, and why" is the whole
    /// diagnosis.
    fn record_intent(&self, args: &serde_json::Value) -> Option<ToolResult> {
        let command = args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor(INTENT_CHANNEL.to_string(), None, None)
            .with_action(
                command.to_string(),
                INTENT_RISK_LEVEL.to_string(),
                // Neither is settled at intent time: the approval gate and the
                // command gate both sit further in. `false`/`false` reads as
                // "not yet", and the result line the same command writes on the
                // way out carries the decided values.
                false,
                false,
            );
        match self.audit.logger.log(&event) {
            Ok(()) => None,
            Err(error) => {
                tracing::error!(
                    sink = %self.audit.sink.display(),
                    %error,
                    "[audit] shell audit sink is not writable; refusing the command (fail-closed) — it was NOT run"
                );
                Some(ToolResult::error(format!(
                    "shell refused: the command was not run because its audit record could not be \
                     written to {} ({error}). Shell execution is deliberately fail-closed on \
                     auditing — free space or fix permissions on that path and retry.",
                    self.audit.sink.display(),
                )))
            }
        }
    }
}

#[async_trait]
impl Tool for AuditedShellTool {
    // --- the guarded execution paths -------------------------------------
    //
    // All three are overridden rather than leaning on the trait's defaults.
    // Overriding only `execute` would still gate every call (the defaults chain
    // down to `self.execute`), but it would drop the caller's `ToolCallOptions`
    // and the TinyAgents `ToolExecutionContext` — and that context carries the
    // per-worker worktree the vendored tool uses as its action dir, so losing it
    // would silently move where commands run.

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if let Some(refusal) = self.record_intent(&args) {
            return Ok(refusal);
        }
        self.inner.execute(args).await
    }

    async fn execute_with_options(
        &self,
        args: serde_json::Value,
        options: ToolCallOptions,
    ) -> anyhow::Result<ToolResult> {
        if let Some(refusal) = self.record_intent(&args) {
            return Ok(refusal);
        }
        self.inner.execute_with_options(args, options).await
    }

    // `Option<&dyn ToolRunContext>`, not the concrete TinyAgents
    // `ToolExecutionContext` this used to name. The tinytools extraction turned
    // the context into a trait so a shared tool vocabulary need not depend on
    // tinyagents (that would be a dependency cycle — tinyagents depends on
    // tinytools). The trait exposes the workspace, the thread id and the output
    // budget and nothing else; the run id, event sink and cancellation token
    // stay harness-internal on purpose.
    //
    // What matters here is unchanged and is the reason this method is
    // overridden at all: the context carries the per-worker worktree the
    // vendored tool uses as its action dir, so it is forwarded whole. Dropping
    // it would silently move where commands run.
    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        options: ToolCallOptions,
        context: Option<&dyn oh::tools::traits::ToolRunContext>,
    ) -> anyhow::Result<ToolResult> {
        if let Some(refusal) = self.record_intent(&args) {
            return Ok(refusal);
        }
        self.inner
            .execute_with_context(args, options, context)
            .await
    }

    // --- pure delegation --------------------------------------------------
    //
    // Every remaining method forwards, so the wrapper cannot drift from the
    // vendored tool's advertised surface when upstream changes one of them.

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    fn supports_markdown(&self) -> bool {
        self.inner.supports_markdown()
    }

    fn permission_level(&self) -> PermissionLevel {
        self.inner.permission_level()
    }

    fn permission_level_with_args(&self, args: &serde_json::Value) -> PermissionLevel {
        self.inner.permission_level_with_args(args)
    }

    fn scope(&self) -> ToolScope {
        self.inner.scope()
    }

    fn category(&self) -> ToolCategory {
        self.inner.category()
    }

    fn is_concurrency_safe(&self, args: &serde_json::Value) -> bool {
        self.inner.is_concurrency_safe(args)
    }

    fn external_effect(&self) -> bool {
        self.inner.external_effect()
    }

    fn external_effect_with_args(&self, args: &serde_json::Value) -> bool {
        self.inner.external_effect_with_args(args)
    }

    // Host metadata this wrapper must not swallow.
    //
    // `Tool` used to carry a typed `generated_runtime_context`, and this
    // decorator forwarded it. The tinytools extraction replaced it with an
    // ERASED pair — `host_extension` for what the tool is, `host_call_extension`
    // for what a particular call is — because the answers are host policy
    // (OpenCompany's generated-tool provenance, OpenHuman's pack-registry
    // handle) and a shared vocabulary has no business naming either. The typed
    // reader is a free function now: `oh::agent::tools::traits`'s
    // `generated_runtime_context`, which downcasts what these return.
    //
    // Both must be forwarded, and forwarding is the whole job of this
    // decorator: a wrapper that answered `None` (the default) would make every
    // wrapped tool look like a tool with no provenance and no pack, silently,
    // to policy that has no other way to ask.
    fn host_extension(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        self.inner.host_extension()
    }

    fn host_call_extension(
        &self,
        args: &serde_json::Value,
    ) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        self.inner.host_call_extension(args)
    }

    fn max_result_size_chars(&self) -> Option<usize> {
        self.inner.max_result_size_chars()
    }

    fn timeout_policy(&self, args: &serde_json::Value) -> ToolTimeout {
        self.inner.timeout_policy(args)
    }

    fn display_label(&self, args: &serde_json::Value) -> Option<String> {
        self.inner.display_label(args)
    }

    fn display_detail(&self, args: &serde_json::Value) -> Option<String> {
        self.inner.display_detail(args)
    }
}

/// Convenience for the tests and for any future caller that has a security
/// policy, a runtime and a [`ShellAudit`] but no reason to know the wrapping is
/// two constructors.
pub fn audited_shell(
    security: Arc<oh::security::SecurityPolicy>,
    runtime: Arc<dyn oh::agent::host_runtime::RuntimeAdapter>,
    audit: ShellAudit,
) -> AuditedShellTool {
    let inner = ShellTool::new(security, runtime, Arc::clone(&audit.logger));
    AuditedShellTool::new(inner, audit)
}

#[cfg(test)]
mod test;
