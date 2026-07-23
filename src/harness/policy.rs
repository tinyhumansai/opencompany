//! [`ApprovalPolicy`] — a manifest `[policy]` → openhuman [`ToolPolicy`] bridge.
//!
//! Manifest `[policy].mode` deliberately uses OpenHuman's own security-tier
//! words — `readonly` / `supervised` / `full` — so the mapping to
//! [`PolicyMode`] is 1:1. On top of the tier the bridge honours the manifest's
//! `always_approve` effect kinds and the per-agent `budget_usd_daily` /
//! `auto_approve_under_usd` thresholds.
//!
//! ## Where approvals actually park (flagged seam)
//!
//! openhuman's [`ToolPolicy`] returns
//! [`ToolPolicyDecision::RequireApproval`](oh::agent::tool_policy::ToolPolicyDecision::RequireApproval),
//! which the session turn loop treats **fail-closed** — it blocks the tool call
//! rather than suspending and resuming it inline. To realise the spec's
//! "park → resolve → resume" flow through opencompany's [`ApprovalGate`] port
//! and journal, the runtime/chat layer (WS3) maps the flagged tool call to an
//! [`Effect`] via [`ApprovalPolicy::effect_for`], parks it on the gate, and
//! re-runs the turn once the operator resolves it. That runtime wiring is the
//! WS3 seam; this module provides the decision + the effect projection.

use async_trait::async_trait;
use openhuman_core::openhuman as oh;

use oh::agent::tool_policy::{ToolPolicy, ToolPolicyDecision, ToolPolicyRequest};

use crate::company::Policy;
use crate::ports::types::{Effect, EffectGroup};

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

/// openhuman [`ToolPolicy`] derived from a company's manifest `[policy]` and a
/// single agent's per-agent budget.
pub struct ApprovalPolicy {
    mode: PolicyMode,
    always_approve: Vec<String>,
    auto_approve_under_usd: Option<f64>,
    /// Per-agent daily spend cap; retained for the runtime budget gate. `None`
    /// leaves budget enforcement to the company-wide `[budget]` ceiling.
    budget_usd_daily: Option<f64>,
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
        }
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
            return ToolPolicyDecision::require_approval(format!(
                "'{tool}' is in the company's always-approve list"
            ));
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
                    ToolPolicyDecision::require_approval(format!(
                        "'{tool}' has an external effect and this desk runs supervised"
                    ))
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

    #[test]
    fn effect_projection_infers_group_and_amount() {
        let p = policy("supervised", &[], None);
        let effect = p.effect_for("pay_invoice", &serde_json::json!({ "amount_usd": 12.5 }));
        assert_eq!(effect.kind, "pay_invoice");
        assert_eq!(effect.group, EffectGroup::Spend);
        assert_eq!(effect.amount_usd, Some(12.5));
    }
}
