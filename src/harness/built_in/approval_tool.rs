//! Explicit agent-to-operator approval requests.
//!
//! This is the general-purpose agent surface for creating an approval. Ordinary
//! tool calls are not converted into approval requests by policy: an agent must
//! call this tool deliberately, explain what it wants to do, and then stop until
//! the operator resolves the card. Specialized tools may still explicitly
//! advertise that their own call stages a concrete approval.

use async_trait::async_trait;
use openhuman_core::openhuman::tools::traits::{PermissionLevel, Tool, ToolResult};
use serde_json::{Value, json};

use crate::harness::policy::{ApprovalRequest, ApprovalRequestQueue};
use crate::ports::types::{Effect, EffectGroup, REQUEST_APPROVAL_EFFECT_KIND};

/// The stable tool/effect name used from the model call through the approval
/// journal and back into the continuation turn.
pub const REQUEST_APPROVAL_TOOL: &str = REQUEST_APPROVAL_EFFECT_KIND;

/// A tool an agent calls when it deliberately wants an operator decision.
pub struct RequestApprovalTool {
    agent: String,
    requests: ApprovalRequestQueue,
}

impl RequestApprovalTool {
    pub fn new(agent: impl Into<String>, requests: ApprovalRequestQueue) -> Self {
        Self {
            agent: agent.into(),
            requests,
        }
    }
}

#[async_trait]
impl Tool for RequestApprovalTool {
    fn name(&self) -> &str {
        REQUEST_APPROVAL_TOOL
    }

    fn description(&self) -> &str {
        "Explicitly ask the operator to approve a proposed action. Use this only when you decide \
         human sign-off is genuinely needed; ordinary tools are not automatically approval-gated. \
         Describe one concrete action and why it needs approval. After calling, stop and wait: do \
         not perform the action until a later turn reports the operator's decision."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short action-oriented title shown on the approval card."
                },
                "question": {
                    "type": "string",
                    "description": "The precise yes/no decision the operator is being asked to make."
                },
                "context": {
                    "type": "string",
                    "description": "Optional evidence, tradeoffs, or consequences the operator needs to decide."
                }
            },
            "required": ["title", "question"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let title = required_text(&args, "title")?.to_string();
        let question = required_text(&args, "question")?.to_string();
        self.requests.push(ApprovalRequest {
            tool: REQUEST_APPROVAL_TOOL.to_string(),
            reason: question,
            effect: Effect {
                kind: REQUEST_APPROVAL_TOOL.to_string(),
                group: EffectGroup::Other,
                amount_usd: None,
                established_thread: false,
                first_time_counterparty: false,
                payload: args,
                agent: Some(self.agent.clone()),
                run_id: None,
            },
        });

        Ok(ToolResult::success(format!(
            "Approval requested: {title}. Stop now and wait for the operator's decision; do not \
             perform the proposed action in this turn."
        )))
    }
}

fn required_text<'a>(args: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`{field}` must be a non-empty string"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn calling_the_tool_queues_one_explicit_request_for_its_agent() {
        let queue = ApprovalRequestQueue::default();
        let tool = RequestApprovalTool::new("finance", queue.clone());
        let args = json!({
            "title": "Send the filing",
            "question": "May I submit the signed filing?",
            "context": "Submission is irreversible."
        });

        let result = tool.execute(args.clone()).await.unwrap();
        assert!(!result.is_error);
        assert!(result.output().contains("Stop now"));

        let drained = queue.drain(8);
        assert_eq!(drained.requests.len(), 1);
        let request = &drained.requests[0];
        assert_eq!(request.tool, REQUEST_APPROVAL_TOOL);
        assert_eq!(request.reason, "May I submit the signed filing?");
        assert_eq!(request.effect.agent.as_deref(), Some("finance"));
        assert_eq!(request.effect.payload, args);
    }

    #[tokio::test]
    async fn blank_required_copy_is_refused_without_queueing_a_card() {
        let queue = ApprovalRequestQueue::default();
        let tool = RequestApprovalTool::new("finance", queue.clone());

        let error = tool
            .execute(json!({ "title": " ", "question": "Proceed?" }))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("`title`"));
        assert!(queue.drain(8).requests.is_empty());
    }
}
