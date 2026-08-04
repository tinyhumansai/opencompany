//! The built-in `feedback` tool: a [`ToolProvider`] decorator.
//!
//! Wraps any inner provider (the stub or the OpenHuman-backed one) and adds a
//! single always-granted `feedback` tool so the brain can self-report when the
//! operator complains mid-conversation. Self-reporting must never be gated, so
//! the `feedback` tool bypasses the manifest grant; every other tool delegates
//! to the inner provider, which enforces grants unchanged.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::feedback::store::FeedbackStore;
use crate::feedback::types::{ConsentMode, FeedbackCategory, FeedbackInput, FeedbackItem};
use crate::ports::EventLog;
use crate::ports::tools::ToolProvider;
use crate::ports::types::{CompanyEvent, CompanyId, ToolCall, ToolResult, ToolSpec};

/// The built-in tool name the brain invokes to file feedback.
pub const FEEDBACK_TOOL: &str = "feedback";

/// The built-in tool name the brain invokes to send an email. Execution is
/// intercepted upstream (in `CycleHostImpl::call_tool`); this provider only
/// advertises the spec.
pub const SEND_EMAIL_TOOL: &str = "send_email";

/// A [`ToolProvider`] that adds the always-granted `feedback` tool on top of an
/// inner provider.
pub struct BuiltinToolProvider {
    inner: Arc<dyn ToolProvider>,
    feedback: Arc<FeedbackStore>,
    events: Arc<dyn EventLog>,
    consent: ConsentMode,
}

impl BuiltinToolProvider {
    /// Wraps `inner`, capturing feedback into `feedback` and logging a
    /// `FeedbackFiled` event through `events`.
    pub fn new(
        inner: Arc<dyn ToolProvider>,
        feedback: Arc<FeedbackStore>,
        events: Arc<dyn EventLog>,
        consent: ConsentMode,
    ) -> Self {
        Self {
            inner,
            feedback,
            events,
            consent,
        }
    }

    /// The `ToolSpec` advertised for the built-in feedback tool.
    fn spec() -> ToolSpec {
        ToolSpec {
            name: FEEDBACK_TOOL.to_string(),
            description: "File feedback about the company's work; captured locally and, with \
                consent, filed as a scrubbed public issue."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "category": {
                        "type": "string",
                        "enum": [
                            "wrong-output", "bug", "missing-capability",
                            "approval-friction", "template-gap", "docs"
                        ]
                    },
                    "note": { "type": "string" },
                    "work_ref": { "type": "string" }
                },
                "required": ["category", "note"]
            }),
        }
    }

    /// The `ToolSpec` advertised for the built-in send_email tool. Execution
    /// is intercepted upstream in `CycleHostImpl::call_tool`; this spec only
    /// advertises the tool to the brain.
    fn send_email_spec() -> ToolSpec {
        ToolSpec {
            name: SEND_EMAIL_TOOL.to_string(),
            description: "Send an email from your company mailbox to a recipient. The first \
                email to a new recipient needs operator approval; replies to people who have \
                emailed you send immediately."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string" },
                    "subject": { "type": "string" },
                    "body": { "type": "string" }
                },
                "required": ["to", "subject", "body"]
            }),
        }
    }

    /// Captures a feedback item from tool arguments: persists it and logs a
    /// `FeedbackFiled` event. Never files (filing is an operator-gated flow).
    async fn capture(&self, company: &CompanyId, call: &ToolCall) -> Result<ToolResult> {
        let category = call
            .args
            .get("category")
            .and_then(|v| serde_json::from_value::<FeedbackCategory>(v.clone()).ok())
            .unwrap_or(FeedbackCategory::WrongOutput);
        let note = call
            .args
            .get("note")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let work_ref = call
            .args
            .get("work_ref")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let input = FeedbackInput {
            category,
            note,
            work_ref,
            template_name: None,
            template_version: None,
        };
        let item = FeedbackItem::capture(input, crate::VERSION, self.consent);
        self.feedback.append(&item).await?;
        self.events
            .append(
                company,
                CompanyEvent::FeedbackFiled {
                    note: item.operator_words.clone(),
                },
            )
            .await?;
        Ok(ToolResult {
            ok: true,
            output: serde_json::json!({ "feedback_id": item.id, "captured": true }),
        })
    }
}

#[async_trait]
impl ToolProvider for BuiltinToolProvider {
    async fn catalog(&self, company: &CompanyId) -> Result<Vec<ToolSpec>> {
        let mut catalog = self.inner.catalog(company).await?;
        catalog.push(Self::spec());
        catalog.push(Self::send_email_spec());
        Ok(catalog)
    }

    async fn invoke(&self, company: &CompanyId, call: ToolCall) -> Result<ToolResult> {
        if call.tool == FEEDBACK_TOOL {
            return self.capture(company, &call).await;
        }
        self.inner.invoke(company, call).await
    }
}

impl std::fmt::Debug for BuiltinToolProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinToolProvider")
            .field("consent", &self.consent)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::feedback::types::ConsentMode;
    use crate::runtime::tools::StubToolProvider;
    use crate::store::FsEventLog;
    use crate::store::paths::Bundle;

    /// The caller must hold the returned handle: it owns the bundle root and
    /// removes it on drop.
    fn wiring() -> (BuiltinToolProvider, Arc<FeedbackStore>, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("oc-btool-")
            .tempdir()
            .expect("tempdir");
        let root = dir.path().to_path_buf();
        let bundle = Bundle::new(root.clone(), &CompanyId::new("acme"));
        let feedback = Arc::new(FeedbackStore::new(&bundle));
        let events: Arc<dyn EventLog> = Arc::new(FsEventLog::new(root.clone()));
        let inner: Arc<dyn ToolProvider> = Arc::new(StubToolProvider::new(vec!["email.*".into()]));
        let provider =
            BuiltinToolProvider::new(inner, feedback.clone(), events, ConsentMode::Manual);
        (provider, feedback, dir)
    }

    #[tokio::test]
    async fn catalog_includes_feedback_tool() {
        let (provider, _fb, _root) = wiring();
        let catalog = provider.catalog(&CompanyId::new("acme")).await.unwrap();
        assert!(catalog.iter().any(|t| t.name == FEEDBACK_TOOL));
    }

    #[tokio::test]
    async fn catalog_advertises_send_email() {
        let (provider, _fb, _root) = wiring();
        let specs = provider.catalog(&CompanyId::new("acme")).await.unwrap();
        let spec = specs
            .iter()
            .find(|s| s.name == "send_email")
            .expect("send_email advertised");
        let req = &spec.input_schema["required"];
        assert!(req.as_array().unwrap().iter().any(|v| v == "to"));
    }

    #[tokio::test]
    async fn feedback_tool_captures_without_grant() {
        let (provider, feedback, _root) = wiring();
        let result = provider
            .invoke(
                &CompanyId::new("acme"),
                ToolCall {
                    tool: FEEDBACK_TOOL.into(),
                    args: serde_json::json!({ "category": "bug", "note": "route broke" }),
                },
            )
            .await
            .unwrap();
        assert!(result.ok);
        // The item was persisted even though `feedback` is not in the grant.
        let items = feedback.list().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].operator_words, "route broke");
        assert_eq!(items[0].category, FeedbackCategory::Bug);
    }

    #[tokio::test]
    async fn non_feedback_tool_delegates_and_enforces_grants() {
        let (provider, _fb, _root) = wiring();
        // Ungranted tool is still rejected by the inner provider.
        let err = provider
            .invoke(
                &CompanyId::new("acme"),
                ToolCall {
                    tool: "payment.send".into(),
                    args: serde_json::Value::Null,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::OpenCompanyError::ToolNotGranted(t) if t == "payment.send"));
    }
}
