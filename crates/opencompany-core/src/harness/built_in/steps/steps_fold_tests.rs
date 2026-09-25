use super::steps_fixtures_tests::*;
use super::*;

// Shape of the fold (unchanged by #411)
// -----------------------------------------------------------------------

#[test]
fn pairs_started_and_completed_into_one_step() {
    let steps = fold_steps(vec![
        started("c1", "mcp_call_tool", Some("Searching the web")),
        completed(
            "c1",
            "mcp_call_tool",
            true,
            "ok",
            Some(serde_json::json!({"server": "brave", "tool": "search"})),
            None,
        ),
    ]);
    assert_eq!(steps.len(), 1, "one step for the pair: {steps:?}");
    assert_eq!(steps[0].kind, TurnStepKind::ToolCall);
    assert_eq!(steps[0].status, TurnStepStatus::Ok);
    assert_eq!(steps[0].label, "Searching the web");
    assert_eq!(steps[0].detail.as_deref(), Some("brave · search"));
    assert_eq!(steps[0].elapsed_ms, Some(42));
}

/// A tool that answers `display_label` however a test needs it to.
struct LabelledTool {
    name: &'static str,
    label: Option<&'static str>,
}

impl LabelledTool {
    fn boxed(name: &'static str, label: Option<&'static str>) -> Box<dyn tinytools::Tool> {
        Box::new(Self { name, label })
    }
}

#[async_trait::async_trait]
impl tinytools::Tool for LabelledTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "a tool"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({ "type": "object" })
    }

    fn display_label(&self, _args: &Value) -> Option<String> {
        self.label.map(str::to_string)
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<tinytools::ToolResult> {
        Ok(tinytools::ToolResult::success("ok"))
    }
}

/// The whole reason [`StepLabels`] exists: the vendored turn loop labels a
/// tool row from the tool's **name**, and never asks the tool what it calls
/// itself.
///
/// If this needle stops matching, upstream has changed how a row is
/// labelled — most likely by consulting `Tool::display_label` at last. Check
/// before deleting anything: `resolve` already defers to a label the loop
/// chose, so a correct upstream makes this shim inert rather than wrong, and
/// it can then go.
#[test]
fn the_vendored_loop_still_labels_a_tool_row_from_its_name_alone() {
    let src = vendored(
        "vendor/openhuman/crates/openhuman-core/src/agent/tinyagents/observability/event_projection.rs",
    );
    // The property, not the spelling. This pinned one exact line
    // (`display_label: Some(humanize_tool_name(tool_name))`) and went red on
    // an upstream bump that only moved it — the loop still derived every
    // label from the name. A needle that breaks on a refactor cries wolf,
    // and a canary nobody trusts gets deleted for the wrong reason.
    assert!(
        src.contains("humanize_tool_name("),
        "the vendored loop no longer derives a tool row's label from its name; find \
         what it derives one from now and pin that instead"
    );
    assert!(
        !src.contains(".display_label()"),
        "the vendored loop now asks the tool for its own label, so `StepLabels` is \
         redundant and should be removed rather than left to shadow the real answer"
    );
}

#[test]
fn step_labels_keep_overrides_and_drop_the_default() {
    // The default is the loop's own humanizer, which Title-Cases *every*
    // word — "Spawn Task", not "Spawn task". Pinned here because the whole
    // map hinges on recognising that string: read it as sentence case and
    // every tool looks like it has an override.
    assert_eq!(humanize_tool_name("spawn_task"), "Spawn Task");

    let labels = StepLabels::from_tools(&[
        LabelledTool::boxed("web_search", Some("Exa web search")),
        // The trait default: humanizing its own name. Not an override.
        LabelledTool::boxed("spawn_task", Some("Spawn Task")),
        LabelledTool::boxed("file_read", None),
        LabelledTool::boxed("file_write", Some("   ")),
    ]);
    let mut kept: Vec<_> = labels
        .0
        .iter()
        .map(|(name, label)| (name.as_str(), label.as_str()))
        .collect();
    kept.sort_unstable();
    assert_eq!(kept, vec![("web_search", "Exa web search")]);
}

/// The end-to-end shape of the bug: the loop hands the timeline the
/// humanized name, and the tool's own label puts it back.
#[test]
fn a_curated_label_replaces_the_loop_s_humanized_name() {
    let labels =
        StepLabels::from_tools(&[LabelledTool::boxed("web_search", Some("Exa web search"))]);
    let steps = fold_steps(
        vec![
            // Exactly what the loop emits: `humanize_tool_name("web_search")`.
            started("c1", "web_search", Some("Web Search")),
            completed("c1", "web_search", true, "ok", None, None),
        ]
        .into_iter()
        .map(|event| labels.apply(event))
        .collect(),
    );
    assert_eq!(steps[0].label, "Exa web search");
}

/// A BYO tenant reads the provider actually wired behind the belt — the half
/// of the fix that branding on the tool *name* could never deliver, since
/// every provider is aliased to the one canonical `web_search`.
#[test]
fn each_provider_s_own_label_reaches_the_timeline_under_one_tool_name() {
    for provider in [
        "Brave web search",
        "Querit web search",
        "SearXNG web search",
    ] {
        let labels = StepLabels::from_tools(&[LabelledTool::boxed("web_search", Some(provider))]);
        let steps = fold_steps(
            vec![
                started("c1", "web_search", Some("Web Search")),
                completed("c1", "web_search", true, "ok", None, None),
            ]
            .into_iter()
            .map(|event| labels.apply(event))
            .collect(),
        );
        assert_eq!(steps[0].label, provider);
    }
}

#[test]
fn a_tool_without_an_override_is_left_alone() {
    let labels =
        StepLabels::from_tools(&[LabelledTool::boxed("web_search", Some("Exa web search"))]);
    let steps = fold_steps(
        vec![
            started("c1", "spawn_task", Some("Spawn task")),
            completed("c1", "spawn_task", true, "ok", None, None),
        ]
        .into_iter()
        .map(|event| labels.apply(event))
        .collect(),
    );
    assert_eq!(steps[0].label, "Spawn task");
}

/// A label the loop *chose* for this call outranks the build-time snapshot.
/// The unavailable-tool row is the live example: it names a tool that never
/// ran, and "Web Search" would hide why the row is there at all.
#[test]
fn a_label_the_loop_chose_survives() {
    let labels =
        StepLabels::from_tools(&[LabelledTool::boxed("web_search", Some("Exa web search"))]);
    let steps = fold_steps(
        vec![
            started("c1", "web_search", Some("Web search (unavailable)")),
            completed("c1", "web_search", false, "", None, None),
        ]
        .into_iter()
        .map(|event| labels.apply(event))
        .collect(),
    );
    assert_eq!(steps[0].label, "Web search (unavailable)");
}

/// Nothing but a tool-call start is touched, so the rewrite cannot perturb
/// the thinking/text folding the rest of this module depends on.
#[test]
fn apply_leaves_every_other_event_untouched() {
    let labels =
        StepLabels::from_tools(&[LabelledTool::boxed("web_search", Some("Exa web search"))]);
    let events = vec![
        thinking("hm"),
        text("hello"),
        completed("c1", "web_search", true, "ok", None, None),
    ];
    let applied: Vec<_> = events
        .clone()
        .into_iter()
        .map(|event| labels.apply(event))
        .collect();
    assert_eq!(fold_steps(applied), fold_steps(events));
}

#[test]
fn label_falls_back_to_humanized_tool_name() {
    let steps = fold_steps(vec![
        started("c1", "spawn_task", None),
        completed("c1", "spawn_task", true, "ok", None, None),
    ]);
    assert_eq!(steps[0].label, "Spawn task");
}

#[test]
fn consecutive_thinking_coalesces_but_text_between_splits() {
    let steps = fold_steps(vec![
        thinking("let"),
        thinking(" me"),
        thinking(" think"),
        text("Here"),
        thinking("more"),
        thinking(" thought"),
    ]);
    let thinking_steps: Vec<_> = steps
        .iter()
        .filter(|s| s.kind == TurnStepKind::Thinking)
        .collect();
    assert_eq!(
        thinking_steps.len(),
        2,
        "two runs (split by the text delta): {steps:?}"
    );
    assert!(thinking_steps.iter().all(|s| s.label == "Thinking"));
    assert!(thinking_steps.iter().all(|s| s.detail.is_none()));
}

#[test]
fn unmatched_started_stays_running() {
    let steps = fold_steps(vec![started("c1", "mcp_call_tool", Some("Searching"))]);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, TurnStepStatus::Running);
    assert_eq!(steps[0].elapsed_ms, None);
}

#[test]
fn caps_at_fifty_with_omission_note() {
    let mut events = Vec::new();
    for i in 0..60 {
        events.push(completed(
            &format!("c{i}"),
            "spawn_task",
            true,
            "ok",
            None,
            None,
        ));
    }
    let steps = fold_steps(events);
    assert_eq!(steps.len(), MAX_STEPS + 1, "50 steps + one omission note");
    let note = steps.last().unwrap();
    assert_eq!(note.kind, TurnStepKind::Note);
    assert_eq!(note.label, "10 more steps omitted");
}

/// A memory-served answer runs zero steps — the tell that distinguishes it
/// from a tool-backed one — so an empty stream folds to an empty timeline.
#[test]
fn empty_stream_folds_to_no_steps() {
    assert!(fold_steps(Vec::new()).is_empty());
}

// -----------------------------------------------------------------------
// #411: a parked call is not a failure
// -----------------------------------------------------------------------

/// The headline of issue #411. A call the approval gate parked is *waiting
/// on a person* — the single most actionable state in the timeline — and it
/// rendered as a crash carrying "Something went wrong with this action."
#[test]
fn a_parked_call_says_so_and_is_not_a_failure() {
    let step = one("send_email", false, &approval_refusal("send_email"), None);

    assert_eq!(step.status, TurnStepStatus::AwaitingApproval);
    assert!(
        !step.status.is_failure(),
        "a parked call must not be counted as failed"
    );
    assert_eq!(
        step.failure, None,
        "waiting on a human is not a failure kind"
    );
    assert_eq!(step.result.as_deref(), Some(AWAITING_APPROVAL_RESULT));
    assert!(
        !step.result.as_deref().unwrap().contains("went wrong"),
        "the generic copy must be gone: {:?}",
        step.result
    );
}

/// The regression this replaces, stated as a property: run the *same*
/// refusal through the classifier the old code used, and it lands on
/// `Unknown` — i.e. on "Something went wrong with this action.". That is
/// why the park needs its own arm ahead of classification, and this test
/// fails the day upstream grows a real arm for it (at which point the
/// special case can go).
#[test]
fn the_classifier_alone_still_cannot_recognise_a_parked_call() {
    let refusal = approval_refusal("send_email");
    assert_eq!(
        oh::tools::status::classify(&refusal, false).class,
        ToolFailureClass::Unknown,
        "if this now classifies, replace the bespoke arm in `complete` with it"
    );
}

/// Both needles are required. A refusal from some *other* tool policy on the
/// same host is a genuine block, not our park, and must keep reading as one.
#[test]
fn only_our_own_policys_park_claims_awaiting_approval() {
    assert!(is_awaiting_approval(&approval_refusal("send_email")));
    assert!(
        !is_awaiting_approval(
            "Blocked: Tool 'shell' requires approval under policy 'some-other-policy'."
        ),
        "another policy's approval block is not our park"
    );
    assert!(
        !is_awaiting_approval("Blocked: Tool 'shell' was denied by policy 'opencompany-approval'."),
        "a hard deny from our policy is a failure, not a park"
    );
}

/// COUPLING: the needle is a string taken from a vendored render, which is
/// the anti-pattern this pins. If OpenHuman rewords
/// `PolicyDenial::ApprovalRequired`, this fails in CI rather than silently
/// returning every parked call to reading as a crash.
#[test]
fn approval_needle_still_appears_in_the_vendored_denial_render() {
    let source =
        vendored("vendor/openhuman/crates/openhuman-core/src/agent/tinyagents/policy_denial.rs");
    assert!(
        source.contains(APPROVAL_REQUIRED_NEEDLE),
        "'{APPROVAL_REQUIRED_NEEDLE}' is gone from PolicyDenial::render — \
         re-derive `is_awaiting_approval` against the new wording"
    );
}

/// COUPLING, the other half: the policy name in the refusal is *ours*, so
/// it is pinned to the impl that emits it rather than to a second literal.
#[test]
fn the_policy_name_the_classifier_keys_on_is_the_one_the_policy_reports() {
    use crate::company::Policy;
    use crate::harness::policy::ApprovalPolicy;
    use oh::agent::tool_policy::ToolPolicy;

    let policy = ApprovalPolicy::new(&Policy::default(), None);
    assert_eq!(policy.name(), POLICY_NAME);
}

// -----------------------------------------------------------------------
// #411: every failure says what it is
// -----------------------------------------------------------------------

/// The three the issue names by hand — unauthorized, timeout, blocked — plus
/// the rest of the taxonomy, each arriving as a typed value the console can
/// switch on instead of prose it has to read.
#[test]
fn each_failure_class_maps_to_its_own_operator_facing_kind() {
    for (class, expected) in [
        (
            ToolFailureClass::BadCredentials,
            TurnStepFailure::Unauthorized,
        ),
        (ToolFailureClass::Timeout, TurnStepFailure::Timeout),
        (
            ToolFailureClass::BlockedByPolicy,
            TurnStepFailure::BlockedByPolicy,
        ),
        (ToolFailureClass::Denied, TurnStepFailure::Declined),
        (ToolFailureClass::ApprovalExpired, TurnStepFailure::Declined),
        (
            ToolFailureClass::MissingPermission,
            TurnStepFailure::MissingPermission,
        ),
        (ToolFailureClass::MissingApp, TurnStepFailure::MissingApp),
        (ToolFailureClass::NotFound, TurnStepFailure::NotFound),
        (ToolFailureClass::Unsupported, TurnStepFailure::Unsupported),
        (
            ToolFailureClass::ServiceUnavailable,
            TurnStepFailure::Unavailable,
        ),
        (
            ToolFailureClass::ModelConnection,
            TurnStepFailure::Unavailable,
        ),
        (ToolFailureClass::Unknown, TurnStepFailure::Failed),
    ] {
        assert_eq!(failure_of(class), expected, "class {class:?}");
    }
}
