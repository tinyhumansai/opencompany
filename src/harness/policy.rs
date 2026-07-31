//! [`ApprovalPolicy`] — a manifest `[policy]` → openhuman [`ToolPolicy`] bridge.
//!
//! Manifest `[policy].mode` deliberately uses OpenHuman's own security-tier
//! words — `readonly` / `supervised` / `full` — so the mapping to
//! [`PolicyMode`] is 1:1. On top of the tier the bridge honours the manifest's
//! `always_approve` effect kinds and the per-agent `budget_usd_daily` /
//! `auto_approve_under_usd` thresholds.
//!
//! ## Where approvals actually park (issue #172)
//!
//! openhuman's [`ToolPolicy`] returns
//! [`ToolPolicyDecision::RequireApproval`](oh::agent::tool_policy::ToolPolicyDecision::RequireApproval),
//! which the session turn loop treats **fail-closed** — it blocks the tool call
//! and feeds the model a refusal rather than suspending and resuming it inline.
//! That refusal was for a long time the *only* trace a gated call left: nothing
//! was ever written to opencompany's [`ApprovalGate`] port or its journal, so
//! the operator's Approvals page stayed empty however many tools an agent
//! parked, and the work silently dead-ended.
//!
//! The bridge is now closed. Every `RequireApproval` this policy returns also
//! projects the flagged call onto an opencompany [`Effect`]
//! ([`ApprovalPolicy::effect_for`]) and pushes it onto the shared
//! [`ApprovalRequestQueue`] carried on
//! [`HarnessDeps`](crate::harness::HarnessDeps). The
//! [`HarnessBrain`](crate::harness::HarnessBrain) drains that queue after the
//! turn and parks each request through
//! [`CycleHost::park_effect`](crate::ports::brain::CycleHost::park_effect), so
//! the request lands in the journal the Approvals page reads and survives a
//! restart. Same cheap-shared-handle pattern as the delegation and MCP-failure
//! queues.
//!
//! **Still out of scope:** resume-after-approval. openhuman resolves the
//! decision inline, so approving a parked tool call records the verdict and
//! clears the queue but does not re-dispatch the tool — the operator re-asks.
//! Suspending and resuming a call inside openhuman's session loop is a separate
//! piece of work.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::agent::tool_policy::{ToolPolicy, ToolPolicyDecision, ToolPolicyRequest};

use crate::company::Policy;
use crate::ports::types::{Effect, EffectGroup};

/// Most approval requests parked out of a single turn. A model that keeps
/// re-trying a blocked tool (openhuman feeds it a refusal and lets it continue)
/// must not be able to flood the operator's queue, so the drain is bounded the
/// same way delegation is.
pub const MAX_APPROVAL_REQUESTS_PER_TURN: usize = 8;

/// The three approval tiers, mirroring OpenHuman's security tiers 1:1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyMode {
    /// Read-only: mutating / external-effect tools are denied outright.
    Readonly,
    /// Supervised: external-effect tools require operator approval.
    Supervised,
    /// Full autonomy: tools run without approval (except `always_approve`).
    Full,
}

impl PolicyMode {
    /// Parses a manifest `[policy].mode` string; unknown values fall back to the
    /// safe `Supervised` default.
    pub fn parse(mode: &str) -> Self {
        match mode.trim().to_ascii_lowercase().as_str() {
            "readonly" => Self::Readonly,
            "full" => Self::Full,
            _ => Self::Supervised,
        }
    }

    /// The openhuman security-tier word this mode maps to (1:1).
    pub fn security_tier(self) -> &'static str {
        match self {
            Self::Readonly => "readonly",
            Self::Supervised => "supervised",
            Self::Full => "full",
        }
    }
}

/// One approval-gated tool call observed during an agent turn: the projected
/// [`Effect`] the operator will see, plus the tool and the policy's own reason
/// for logging.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    /// The tool the agent tried to call.
    pub tool: String,
    /// Why the policy flagged it (the same wording openhuman feeds the model).
    pub reason: String,
    /// The projected effect to park on the gate.
    pub effect: Effect,
}

/// A shared, in-memory queue of approval-gated tool calls — the exact
/// [`DelegationQueue`](crate::harness::orchestrator::DelegationQueue) /
/// [`McpFailureQueue`](crate::harness::mcp_probe::McpFailureQueue) pattern.
/// Cheap to [`Clone`] (a shared handle); the [`ApprovalPolicy`] installed on
/// every roster agent and the [`HarnessBrain`](crate::harness::HarnessBrain)
/// that drains it see the same queue because
/// [`HarnessDeps`](crate::harness::HarnessDeps) clones share this handle.
#[derive(Clone, Default)]
pub struct ApprovalRequestQueue {
    inner: Arc<Mutex<Vec<ApprovalRequest>>>,
}

impl ApprovalRequestQueue {
    /// Records a gated call, ignoring one already queued for the same tool and
    /// arguments.
    ///
    /// openhuman blocks the call but lets the turn continue, so a model that
    /// re-tries the same tool would otherwise park the identical request several
    /// times over and show the operator a queue of duplicates.
    pub fn push(&self, request: ApprovalRequest) {
        let mut guard = self.inner.lock().expect("approval request queue");
        if guard.iter().any(|q| {
            q.effect.kind == request.effect.kind && q.effect.payload == request.effect.payload
        }) {
            return;
        }
        guard.push(request);
    }

    /// Empties the queue (called before a turn so a request from a prior turn —
    /// or from a workflow run that shares these deps — never leaks into it).
    pub fn clear(&self) {
        self.inner.lock().expect("approval request queue").clear();
    }

    /// Drains up to `cap` queued requests (FIFO) and discards the rest, so one
    /// turn can never flood the operator's queue.
    pub fn drain(&self, cap: usize) -> Vec<ApprovalRequest> {
        let mut guard = self.inner.lock().expect("approval request queue");
        let take = guard.len().min(cap);
        let drained: Vec<ApprovalRequest> = guard.drain(..take).collect();
        guard.clear();
        drained
    }

    /// The number of queued requests (test/observability).
    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.inner.lock().expect("approval request queue").len()
    }
}

/// openhuman [`ToolPolicy`] derived from a company's manifest `[policy]` and a
/// single agent's per-agent budget.
pub struct ApprovalPolicy {
    mode: PolicyMode,
    always_approve: Vec<String>,
    auto_approve_under_usd: Option<f64>,
    /// Per-agent daily spend cap; retained for the runtime budget gate. `None`
    /// leaves budget enforcement to the company-wide `[budget]` ceiling.
    budget_usd_daily: Option<f64>,
    /// Where a `RequireApproval` decision is recorded so the runtime can park it
    /// (issue #172). The default is a private queue nobody drains, which keeps
    /// every non-harness construction site (and every test) behaving exactly as
    /// before; `build_roster` installs the shared one off
    /// [`HarnessDeps`](crate::harness::HarnessDeps).
    requests: ApprovalRequestQueue,
}

impl ApprovalPolicy {
    /// Builds a policy from the manifest `[policy]` block and an agent's
    /// `budget_usd_daily`.
    pub fn new(policy: &Policy, budget_usd_daily: Option<f64>) -> Self {
        Self {
            mode: PolicyMode::parse(&policy.mode),
            always_approve: policy.always_approve.clone(),
            auto_approve_under_usd: policy.auto_approve_under_usd,
            budget_usd_daily,
            requests: ApprovalRequestQueue::default(),
        }
    }

    /// Installs the shared queue every `RequireApproval` decision is recorded on,
    /// so the brain can park the request after the turn (issue #172).
    pub fn with_requests(mut self, requests: ApprovalRequestQueue) -> Self {
        self.requests = requests;
        self
    }

    /// The resolved tier.
    pub fn mode(&self) -> PolicyMode {
        self.mode
    }

    /// The per-agent daily budget, if any.
    pub fn budget_usd_daily(&self) -> Option<f64> {
        self.budget_usd_daily
    }

    /// Whether `kind` is in the manifest's `always_approve` list. Matches either
    /// the exact dotted kind or a leading segment (so `payment` matches
    /// `payment.send`).
    fn always_requires_approval(&self, kind: &str) -> bool {
        self.always_approve
            .iter()
            .any(|entry| entry == kind || kind.starts_with(&format!("{entry}.")))
    }

    /// Best-effort USD amount carried by a tool call's arguments, from either an
    /// `amount_usd` or `amount` field.
    fn amount_usd(args: &serde_json::Value) -> Option<f64> {
        args.get("amount_usd")
            .or_else(|| args.get("amount"))
            .and_then(|v| v.as_f64())
    }

    /// Project a flagged tool call onto an opencompany [`Effect`] so the runtime
    /// can park it on the [`ApprovalGate`](crate::ports::ApprovalGate). The tool
    /// name becomes the dotted effect `kind`; the group and amount are inferred
    /// best-effort.
    pub fn effect_for(&self, tool_name: &str, args: &serde_json::Value) -> Effect {
        Effect {
            kind: tool_name.to_string(),
            group: classify_group(tool_name),
            amount_usd: Self::amount_usd(args),
            established_thread: false,
            first_time_counterparty: false,
            payload: args.clone(),
        }
    }

    /// The one construction site for a `RequireApproval` decision (issue #172):
    /// record the projected effect on the shared queue so the brain can park it
    /// after the turn, then return the decision openhuman blocks the call with.
    ///
    /// Every `RequireApproval` arm of [`check`](ToolPolicy::check) goes through
    /// here — a decision that skipped it would refuse the tool without ever
    /// reaching the operator, which is exactly the bug this closes.
    fn require_approval(
        &self,
        tool: &str,
        args: &serde_json::Value,
        reason: String,
    ) -> ToolPolicyDecision {
        self.requests.push(ApprovalRequest {
            tool: tool.to_string(),
            reason: reason.clone(),
            effect: self.effect_for(tool, args),
        });
        log::debug!(
            "[approval] tool '{tool}' requires operator approval — queued to park ({reason})"
        );
        ToolPolicyDecision::require_approval(reason)
    }
}

#[async_trait]
impl ToolPolicy for ApprovalPolicy {
    fn name(&self) -> &str {
        "opencompany-approval"
    }

    async fn check(&self, request: &ToolPolicyRequest) -> ToolPolicyDecision {
        let tool = request.tool_name.as_str();

        // `always_approve` wins over everything, including Full autonomy.
        if self.always_requires_approval(tool) {
            return self.require_approval(
                tool,
                &request.arguments,
                format!("'{tool}' is in the company's always-approve list"),
            );
        }

        // Auto-approve small spends under the configured threshold.
        if let (Some(threshold), Some(amount)) = (
            self.auto_approve_under_usd,
            Self::amount_usd(&request.arguments),
        ) && amount < threshold
        {
            return ToolPolicyDecision::Allow;
        }

        let external = is_external_effect(tool);
        match self.mode {
            PolicyMode::Full => ToolPolicyDecision::Allow,
            PolicyMode::Supervised => {
                if external {
                    self.require_approval(
                        tool,
                        &request.arguments,
                        format!("'{tool}' has an external effect and this desk runs supervised"),
                    )
                } else {
                    ToolPolicyDecision::Allow
                }
            }
            PolicyMode::Readonly => {
                if external {
                    ToolPolicyDecision::deny(format!(
                        "'{tool}' mutates or reaches outside; this desk is read-only"
                    ))
                } else {
                    ToolPolicyDecision::Allow
                }
            }
        }
    }
}

/// Heuristic: does this tool mutate state or reach an external counterparty?
///
/// Best-effort classification by name — openhuman's [`ToolPolicy`] surface hands
/// the bridge only the tool name and arguments, not the tool's own
/// external-effect flag. Unknown tools are treated as external (fail-safe).
fn is_external_effect(tool_name: &str) -> bool {
    // The orchestrator's in-cycle delegation tools (`spawn_task`,
    // `delegate_to_desk`) enqueue internal work the harness brain drains this
    // turn — a task card or a hand-off to a desk's lead — never an external
    // effect. Without this, the default `supervised` policy would park them and
    // `readonly` would deny them, breaking in-cycle delegation. (Issue #53.)
    if crate::harness::orchestrator::is_delegation_tool(tool_name) {
        return false;
    }
    // An MCP tool call can perform any effect advertised by a third-party
    // server. Treat it as external even if future prefix rules become broader.
    if tool_name.eq_ignore_ascii_case("mcp_registry_tool_call") {
        return true;
    }
    // The media catalog is a read-only GET (issue #109): listing models spends
    // nothing and must never park for approval, even though its name does not
    // start with a read-only prefix. The `media_generate_*` tools are NOT listed
    // here — they spend real money and fall through to the external-effect
    // default, so they park under supervised / deny under readonly.
    if tool_name.eq_ignore_ascii_case("media_list_models") {
        return false;
    }
    // The Composio read tools (issue #110) are read-only GETs: listing toolkits,
    // connections, or action schemas reaches no third party and must never park
    // for approval, even though the `composio_*` name has no read-only prefix.
    // `composio_authorize` / `composio_execute` are NOT listed here — they begin
    // an OAuth handoff / run a real action, so they fall through to the external-
    // effect default (park under supervised, deny under readonly).
    if matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "composio_list_toolkits" | "composio_list_connections" | "composio_list_tools"
    ) {
        return false;
    }
    const READ_ONLY_PREFIXES: &[&str] = &[
        "read",
        "list",
        "get",
        "search",
        "recall",
        "query",
        "peek",
        "inspect",
        "view",
        "memory_recall",
        "memory_search",
    ];
    let name = tool_name.to_ascii_lowercase();
    !READ_ONLY_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Map a tool name onto the supervised [`EffectGroup`] taxonomy.
fn classify_group(tool_name: &str) -> EffectGroup {
    let name = tool_name.to_ascii_lowercase();
    if name == "mcp_registry_tool_call" {
        EffectGroup::Other
    } else if name == "composio_authorize" {
        // Beginning an OAuth handoff establishes an account identity for the
        // company (issue #110) — an identity effect, parked before it lands.
        EffectGroup::Identity
    } else if name == "composio_execute" {
        // Running a Composio action reaches a third-party account (send an
        // email, post a message, open a PR) — a send effect. Placed before the
        // generic `contains` heuristics so the slug can't be misclassified.
        EffectGroup::Send
    } else if name.starts_with("media_generate") {
        // Image/video generation is billed by the backend on submit (issue
        // #109), so it is a spend effect — parked for approval before money
        // moves. (`media_list_models` is read-only and never reaches here.)
        EffectGroup::Spend
    } else if name.contains("pay") || name.contains("transfer") || name.starts_with("spend") {
        EffectGroup::Spend
    } else if name.contains("email") || name.contains("send") || name.contains("message") {
        EffectGroup::Send
    } else if name.contains("sign") || name.contains("file") {
        EffectGroup::Sign
    } else if name.contains("publish") || name.contains("post") {
        EffectGroup::Publish
    } else if name.contains("hire") || name.contains("contract") {
        EffectGroup::Hire
    } else if name.contains("identity") || name.contains("handle") {
        EffectGroup::Identity
    } else {
        EffectGroup::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oh::agent::tool_policy::{ToolCallContext, ToolPolicyRequest};

    fn policy(mode: &str, always: &[&str], auto_under: Option<f64>) -> ApprovalPolicy {
        let p = Policy {
            mode: mode.to_string(),
            always_approve: always.iter().map(|s| s.to_string()).collect(),
            auto_approve_under_usd: auto_under,
        };
        ApprovalPolicy::new(&p, Some(25.0))
    }

    fn request(tool: &str, args: serde_json::Value) -> ToolPolicyRequest {
        let ctx = ToolCallContext::session("s", "chat", "ceo", "call-1", 0);
        ToolPolicyRequest::new(tool, args, ctx)
    }

    #[test]
    fn mode_maps_one_to_one_to_security_tiers() {
        assert_eq!(PolicyMode::parse("readonly").security_tier(), "readonly");
        assert_eq!(
            PolicyMode::parse("supervised").security_tier(),
            "supervised"
        );
        assert_eq!(PolicyMode::parse("full").security_tier(), "full");
        // Unknown falls back to supervised.
        assert_eq!(PolicyMode::parse("bogus"), PolicyMode::Supervised);
    }

    #[tokio::test]
    async fn full_allows_but_always_approve_still_parks() {
        let p = policy("full", &["payment"], None);
        assert_eq!(
            p.check(&request("write_file", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow
        );
        assert!(matches!(
            p.check(&request("payment.send", serde_json::json!({})))
                .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    #[tokio::test]
    async fn supervised_requires_approval_for_external_effects() {
        let p = policy("supervised", &[], None);
        assert!(matches!(
            p.check(&request("send_email", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(
            p.check(&request("read_file", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow
        );
    }

    #[tokio::test]
    async fn supervised_parks_mcp_tool_calls_as_external_other_effects() {
        let p = policy("supervised", &[], None);
        let args = serde_json::json!({
            "server_id": "server-1",
            "tool_name": "echo",
            "arguments": {"text": "hello"}
        });
        assert!(matches!(
            p.check(&request("mcp_registry_tool_call", args.clone()))
                .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(
            p.effect_for("mcp_registry_tool_call", &args).group,
            EffectGroup::Other
        );
    }

    #[tokio::test]
    async fn readonly_denies_mutations_allows_reads() {
        let p = policy("readonly", &[], None);
        assert!(matches!(
            p.check(&request("publish_post", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Deny { .. }
        ));
        assert_eq!(
            p.check(&request("list_files", serde_json::json!({}))).await,
            ToolPolicyDecision::Allow
        );
    }

    #[tokio::test]
    async fn auto_approve_under_threshold_allows_small_spends() {
        let p = policy("supervised", &[], Some(5.0));
        // $3 spend is under the $5 threshold → allowed even though it's external.
        assert_eq!(
            p.check(&request(
                "pay_invoice",
                serde_json::json!({ "amount_usd": 3.0 })
            ))
            .await,
            ToolPolicyDecision::Allow
        );
        // $9 spend exceeds the threshold → requires approval.
        assert!(matches!(
            p.check(&request(
                "pay_invoice",
                serde_json::json!({ "amount_usd": 9.0 })
            ))
            .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }

    /// Media generation (issue #109): the paid `media_generate_*` tools park
    /// under supervised and deny under readonly (external spend effect), while
    /// the read-only `media_list_models` catalog GET is always allowed.
    #[tokio::test]
    async fn media_generate_parks_supervised_and_denies_readonly_but_list_is_read_only() {
        let supervised = policy("supervised", &[], None);
        for tool in ["media_generate_image", "media_generate_video"] {
            assert!(
                matches!(
                    supervised
                        .check(&request(tool, serde_json::json!({})))
                        .await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "{tool} must park under supervised"
            );
        }
        // The catalog GET is read-only — allowed even under supervised.
        assert_eq!(
            supervised
                .check(&request("media_list_models", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );

        let readonly = policy("readonly", &[], None);
        assert!(
            matches!(
                readonly
                    .check(&request("media_generate_image", serde_json::json!({})))
                    .await,
                ToolPolicyDecision::Deny { .. }
            ),
            "media_generate must be denied under readonly"
        );
        // Even a read-only desk can list the model catalog.
        assert_eq!(
            readonly
                .check(&request("media_list_models", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );
    }

    /// Paid generation classifies as a spend effect (issue #109).
    #[test]
    fn media_generate_classifies_as_spend() {
        let p = policy("supervised", &[], None);
        assert_eq!(
            p.effect_for("media_generate_image", &serde_json::json!({}))
                .group,
            EffectGroup::Spend
        );
        assert_eq!(
            p.effect_for("media_generate_video", &serde_json::json!({}))
                .group,
            EffectGroup::Spend
        );
    }

    /// Per-tenant Composio (issue #110): the read tools are read-only (allowed
    /// even under supervised/readonly), while `composio_authorize` /
    /// `composio_execute` are external — parked under supervised, denied under
    /// readonly.
    #[tokio::test]
    async fn composio_reads_allowed_but_authorize_execute_park_or_deny() {
        let supervised = policy("supervised", &[], None);
        for tool in [
            "composio_list_toolkits",
            "composio_list_connections",
            "composio_list_tools",
        ] {
            assert_eq!(
                supervised
                    .check(&request(tool, serde_json::json!({})))
                    .await,
                ToolPolicyDecision::Allow,
                "{tool} is read-only and must be allowed"
            );
        }
        for tool in ["composio_authorize", "composio_execute"] {
            assert!(
                matches!(
                    supervised
                        .check(&request(tool, serde_json::json!({})))
                        .await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "{tool} must park under supervised"
            );
        }

        let readonly = policy("readonly", &[], None);
        // A read-only desk may still browse the Composio surface.
        assert_eq!(
            readonly
                .check(&request("composio_list_connections", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );
        for tool in ["composio_authorize", "composio_execute"] {
            assert!(
                matches!(
                    readonly.check(&request(tool, serde_json::json!({}))).await,
                    ToolPolicyDecision::Deny { .. }
                ),
                "{tool} must be denied under readonly"
            );
        }
    }

    /// Composio effect groups (issue #110): authorize is an Identity effect,
    /// execute is a Send effect — pinned before the generic `contains`
    /// heuristics could misclassify the slug.
    #[test]
    fn composio_classifies_authorize_identity_and_execute_send() {
        let p = policy("supervised", &[], None);
        assert_eq!(
            p.effect_for("composio_authorize", &serde_json::json!({}))
                .group,
            EffectGroup::Identity
        );
        assert_eq!(
            p.effect_for("composio_execute", &serde_json::json!({}))
                .group,
            EffectGroup::Send
        );
    }

    #[test]
    fn effect_projection_infers_group_and_amount() {
        let p = policy("supervised", &[], None);
        let effect = p.effect_for("pay_invoice", &serde_json::json!({ "amount_usd": 12.5 }));
        assert_eq!(effect.kind, "pay_invoice");
        assert_eq!(effect.group, EffectGroup::Spend);
        assert_eq!(effect.amount_usd, Some(12.5));
    }

    // --- The park queue (issue #172) ----------------------------------------

    fn queued_policy(mode: &str, always: &[&str]) -> (ApprovalPolicy, ApprovalRequestQueue) {
        let queue = ApprovalRequestQueue::default();
        (
            policy(mode, always, None).with_requests(queue.clone()),
            queue,
        )
    }

    /// The core of #172: a `RequireApproval` decision no longer evaporates into
    /// the model's transcript — it is recorded, with the call projected onto the
    /// effect the operator will see, so the runtime can park it.
    #[tokio::test]
    async fn require_approval_records_the_request_to_park() {
        let (p, queue) = queued_policy("supervised", &[]);
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" });
        assert!(matches!(
            p.check(&request("composio_execute", args.clone())).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        let queued = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(queued.len(), 1, "the gated call was recorded");
        assert_eq!(queued[0].tool, "composio_execute");
        assert_eq!(queued[0].effect.kind, "composio_execute");
        assert_eq!(queued[0].effect.group, EffectGroup::Send);
        assert_eq!(queued[0].effect.payload, args);
        assert!(
            queued[0].reason.contains("supervised"),
            "the operator-facing reason rides along: {}",
            queued[0].reason
        );
    }

    /// `always_approve` parks regardless of tier — including under `full` — so
    /// that arm has to record its request too.
    #[tokio::test]
    async fn always_approve_records_the_request_even_under_full_autonomy() {
        let (p, queue) = queued_policy("full", &["payment"]);
        assert!(matches!(
            p.check(&request(
                "payment.send",
                serde_json::json!({ "amount_usd": 40.0 })
            ))
            .await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        let queued = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].effect.kind, "payment.send");
        assert_eq!(queued[0].effect.amount_usd, Some(40.0));
    }

    /// Allowed and denied calls leave the queue alone: only a call actually
    /// waiting on the operator may reach the Approvals page.
    #[tokio::test]
    async fn allow_and_deny_record_nothing() {
        let (supervised, allow_queue) = queued_policy("supervised", &[]);
        assert_eq!(
            supervised
                .check(&request("read_file", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Allow
        );
        assert_eq!(allow_queue.queued(), 0, "an allowed call parks nothing");

        let (readonly, deny_queue) = queued_policy("readonly", &[]);
        assert!(matches!(
            readonly
                .check(&request("publish_post", serde_json::json!({})))
                .await,
            ToolPolicyDecision::Deny { .. }
        ));
        assert_eq!(
            deny_queue.queued(),
            0,
            "a denied call is refused outright, never parked"
        );
    }

    /// openhuman blocks a gated call but lets the turn continue, so a model that
    /// keeps re-trying the same tool must not stack up duplicate approvals.
    #[tokio::test]
    async fn a_retried_call_is_recorded_once() {
        let (p, queue) = queued_policy("supervised", &[]);
        let args = serde_json::json!({ "tool_slug": "GMAIL_SEND_EMAIL" });
        for _ in 0..3 {
            let _ = p.check(&request("composio_execute", args.clone())).await;
        }
        assert_eq!(queue.queued(), 1, "the same call parks once");

        // A different call to the same tool is a distinct request.
        let _ = p
            .check(&request(
                "composio_execute",
                serde_json::json!({ "tool_slug": "SLACK_POST" }),
            ))
            .await;
        assert_eq!(queue.queued(), 2);
    }

    /// The drain is capped, so a runaway turn can't flood the operator's queue.
    #[tokio::test]
    async fn the_drain_is_capped_and_empties_the_queue() {
        let (p, queue) = queued_policy("supervised", &[]);
        for i in 0..(MAX_APPROVAL_REQUESTS_PER_TURN + 4) {
            let _ = p
                .check(&request(
                    "composio_execute",
                    serde_json::json!({ "tool_slug": format!("TOOL_{i}") }),
                ))
                .await;
        }
        let drained = queue.drain(MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(drained.len(), MAX_APPROVAL_REQUESTS_PER_TURN);
        assert_eq!(queue.queued(), 0, "the overflow is discarded, not carried");
    }

    /// A queue nobody installed stays inert — the default policy behaves exactly
    /// as it did before #172 for every non-harness construction site.
    #[tokio::test]
    async fn a_policy_without_a_shared_queue_still_decides_normally() {
        let p = policy("supervised", &[], None);
        assert!(matches!(
            p.check(&request("send_email", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    }
}
