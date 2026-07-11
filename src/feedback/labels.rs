//! GitHub label taxonomy (`docs/spec/feedback-loop/triage.md`).
//!
//! Every filed issue carries `feedback` plus one label from each axis:
//! `type/`, `area/`, `sev/`, and `source/`. Agent-filed issues always carry
//! `source/agent-filed` so triage can weight them.

use crate::feedback::types::{FeedbackCategory, FeedbackItem};

/// Builds the full label set for an agent-filed feedback issue.
///
/// The `type/` label follows the category; `area/` defaults per category (or
/// `template:<name>` for a template gap with a known template); `sev/` defaults
/// to `annoyance`; `source/` is always `agent-filed`.
pub fn labels_for(item: &FeedbackItem) -> Vec<String> {
    vec![
        "feedback".to_string(),
        format!("type/{}", item.category.as_str()),
        format!("area/{}", area_for(item)),
        "sev/annoyance".to_string(),
        "source/agent-filed".to_string(),
    ]
}

/// The owning surface for a feedback item.
fn area_for(item: &FeedbackItem) -> String {
    if item.category == FeedbackCategory::TemplateGap
        && let Some(name) = &item.template_name
        && !name.trim().is_empty()
    {
        return format!("template:{name}");
    }
    match item.category {
        FeedbackCategory::WrongOutput => "brain",
        FeedbackCategory::Bug | FeedbackCategory::ApprovalFriction => "runtime",
        FeedbackCategory::MissingCapability | FeedbackCategory::TemplateGap => "product",
        FeedbackCategory::Docs => "product",
    }
    .to_string()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::feedback::types::{ConsentMode, FeedbackInput};

    fn item(category: FeedbackCategory, template: Option<&str>) -> FeedbackItem {
        FeedbackItem::capture(
            FeedbackInput {
                category,
                note: "n".into(),
                work_ref: None,
                template_name: template.map(str::to_string),
                template_version: None,
            },
            "0.1.0",
            ConsentMode::Auto,
        )
    }

    #[test]
    fn labels_cover_all_axes_and_source() {
        let labels = labels_for(&item(FeedbackCategory::Bug, None));
        assert!(labels.contains(&"feedback".to_string()));
        assert!(labels.contains(&"type/bug".to_string()));
        assert!(labels.contains(&"area/runtime".to_string()));
        assert!(labels.iter().any(|l| l.starts_with("sev/")));
        assert!(labels.contains(&"source/agent-filed".to_string()));
    }

    #[test]
    fn template_gap_names_the_template_area() {
        let labels = labels_for(&item(
            FeedbackCategory::TemplateGap,
            Some("marketing_agency"),
        ));
        assert!(labels.contains(&"area/template:marketing_agency".to_string()));
    }

    #[test]
    fn wrong_output_owns_the_brain() {
        let labels = labels_for(&item(FeedbackCategory::WrongOutput, None));
        assert!(labels.contains(&"area/brain".to_string()));
    }
}
