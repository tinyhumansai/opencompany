use super::*;

use super::operator_test_support_3::*;

/// #185: the correlation key rides the SSE stream when — and only when — the
/// event carries one. Both directions matter: its presence is what lets a
/// live console route a frame to the right task, and its absence is what
/// keeps the legacy shape intact for every ordinary chat reply.
#[test]
fn projects_task_id_only_when_the_event_is_correlated() {
    let reply = super::project_event(&stored(CompanyEvent::AgentReply {
        audience: Vec::new(),
        mentions: Vec::new(),
        mention_depth: 0,
        parent: None,
        task_id: Some("t-1".into()),
        outputs: Vec::new(),
        chat_id: "t-1".into(),
        agent_id: "ceo".into(),
        text: "on it".into(),
        steps: Vec::new(),
    }))
    .expect("agent_reply is an attention signal");
    assert_eq!(reply["taskId"], serde_json::json!("t-1"));

    let failure = super::project_event(&stored(CompanyEvent::McpCallFailed {
        task_id: Some("t-1".into()),
        server: "gh".into(),
        tool: "issues".into(),
        status: "credential_required".into(),
        message: "needs auth".into(),
    }))
    .expect("mcp_call_failed is an attention signal");
    assert_eq!(failure["taskId"], serde_json::json!("t-1"));

    let uncorrelated = super::project_event(&stored(CompanyEvent::McpCallFailed {
        task_id: None,
        server: "gh".into(),
        tool: "issues".into(),
        status: "credential_required".into(),
        message: "needs auth".into(),
    }))
    .expect("mcp_call_failed is an attention signal");
    assert!(uncorrelated.get("taskId").is_none());
}

/// #185/#377: the dispatch terminal projects the structural fields, plus
/// the conversation the card was raised from. `column` is the one that
/// matters most — it is how a console tells a clean finish from a cancelled
/// or failed run — and `chatId` is what says which channel it belongs in.
#[test]
fn projects_desk_task_completed_with_every_field() {
    let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".into(),
        desk: "engineer".into(),
        output: "shipped".into(),
        column: "in_review".into(),
        artifact_ids: Vec::new(),
        origin_chat_id: Some("engineering".into()),
        origin_parent: None,
    }))
    .expect("desk_task_completed is an attention signal");
    assert_eq!(v["type"], serde_json::json!("desk_task_completed"));
    assert_eq!(v["taskId"], serde_json::json!("t-1"));
    assert_eq!(v["desk"], serde_json::json!("engineer"));
    assert_eq!(v["column"], serde_json::json!("in_review"));
    assert_eq!(v["chatId"], serde_json::json!("engineering"));
    // The envelope's own keys still ride along — the console mints the
    // marker's identity from `seq` (issue #483's mechanism), so losing it
    // here would silently disable the reload dedupe.
    assert!(v.get("seq").is_some(), "{v}");
    assert!(v.get("atMillis").is_some(), "{v}");
}

/// Issue #377: the run's prose is **not** on this frame.
///
/// The relay bubble (#151) already carries the agent's words into the same
/// channel this marker lands in. Projecting `output` here as well would put
/// one run's text into one conversation twice, and dropping it at the
/// projection is what stops any later reader from reintroducing that.
#[test]
fn desk_task_completed_does_not_project_the_runs_prose() {
    let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".into(),
        desk: "engineer".into(),
        output: "the whole reply, verbatim".into(),
        column: "in_review".into(),
        artifact_ids: Vec::new(),
        origin_chat_id: Some("engineering".into()),
        origin_parent: None,
    }))
    .expect("desk_task_completed is an attention signal");
    assert!(v.get("output").is_none(), "{v}");
    assert!(
        !v.to_string().contains("the whole reply"),
        "the prose must not reach the wire under any key: {v}"
    );
}

/// Issue #377: a card nobody raised from a conversation omits `chatId`
/// rather than sending null — so "board-created" is a presence check on the
/// console, the same shape `approval_parked` uses for a page-only approval.
#[test]
fn desk_task_completed_omits_the_chat_id_for_a_board_created_card() {
    let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".into(),
        desk: "engineer".into(),
        output: "shipped".into(),
        column: "in_review".into(),
        artifact_ids: Vec::new(),
        origin_chat_id: None,
        origin_parent: None,
    }))
    .expect("desk_task_completed is an attention signal");
    assert!(v.get("chatId").is_none(), "{v}");
    assert_eq!(v["column"], serde_json::json!("in_review"));
}

/// Issue #1890 B: the thread inside the channel, on exactly the terms
/// `chatId` rides on.
///
/// Stringified, because the console keys threads by message id and a
/// message id is a string there — `chat/history` renders the same root the
/// same way, and the two must agree or the marker would render inline live
/// and jump into a thread on reload.
#[test]
fn desk_task_completed_projects_the_thread_its_card_was_raised_in() {
    let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".into(),
        desk: "engineer".into(),
        output: "shipped".into(),
        column: "in_review".into(),
        artifact_ids: Vec::new(),
        origin_chat_id: Some("engineering".into()),
        origin_parent: Some(crate::ports::types::EventSeq::new(41)),
    }))
    .expect("desk_task_completed is an attention signal");
    assert_eq!(v["chatId"], serde_json::json!("engineering"));
    assert_eq!(v["parentId"], serde_json::json!("41"));
}

/// A card raised straight into a channel omits `parentId` rather than
/// sending null — the same presence-check shape `chatId` takes, so the
/// console reads "channel level" without a null check.
#[test]
fn desk_task_completed_omits_the_parent_for_a_channel_level_card() {
    let v = super::project_event(&stored(CompanyEvent::DeskTaskCompleted {
        task_id: "t-1".into(),
        desk: "engineer".into(),
        output: "shipped".into(),
        column: "in_review".into(),
        artifact_ids: Vec::new(),
        origin_chat_id: Some("engineering".into()),
        origin_parent: None,
    }))
    .expect("desk_task_completed is an attention signal");
    assert_eq!(v["chatId"], serde_json::json!("engineering"));
    assert!(v.get("parentId").is_none(), "{v}");
}

#[test]
fn projects_task_dispatched() {
    let v = super::project_event(&stored(CompanyEvent::TaskDispatched {
        task_id: "t-42".into(),
        run_id: None,
        origin_chat_id: None,
        origin_parent: None,
    }))
    .expect("task_dispatched is an attention signal");
    assert_eq!(v["type"], "task_dispatched");
    assert_eq!(v["taskId"], "t-42");
    assert!(
        v.get("chatId").is_none() && v.get("parentId").is_none(),
        "a board-created dispatch belongs to no conversation: {v}"
    );
}

/// A dispatch raised at CHANNEL level names the desk and no thread.
///
/// A distinct branch from the threaded case, in the projection and in the
/// console's keying alike: `parentId` absent beside a present `chatId` means
/// the channel itself, while absent beside an absent `chatId` means no
/// conversation at all. Only the threaded and the board-created cases were
/// covered, so a regression that dropped channel-level origins would have gone
/// unnoticed (tinysweeper, #2369).
#[test]
fn a_dispatch_raised_in_a_channel_names_the_channel_and_no_thread() {
    let v = super::project_event(&stored(CompanyEvent::TaskDispatched {
        task_id: "t-45".into(),
        run_id: Some("r-2".into()),
        origin_chat_id: Some("order_ops".into()),
        origin_parent: None,
    }))
    .expect("task_dispatched is an attention signal");

    assert_eq!(v["chatId"], "order_ops", "the desk that asked");
    assert!(
        v.get("parentId").is_none(),
        "no thread: absent beside a present chatId is the channel itself, and \
         a null would read as a thread whose root is nothing: {v}"
    );
}

/// A dispatch's origin survives the round trip through the journal.
///
/// The fields are additive (`serde(default)` + `skip_serializing_if`), and that
/// combination is easy to get subtly wrong: a line written before they existed
/// must still replay, and a dispatch that carries no conversation must
/// serialize exactly as it did before — while one that does must come back with
/// both halves intact (tinysweeper, #2369).
#[test]
fn a_dispatch_origin_survives_serialization() {
    let raised = CompanyEvent::TaskDispatched {
        task_id: "t-42".into(),
        run_id: Some("r-1".into()),
        origin_chat_id: Some("main".into()),
        origin_parent: Some(crate::ports::types::EventSeq::new(50)),
    };
    let wire = serde_json::to_string(&raised).expect("serializes");
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(&wire).expect("round trips"),
        raised,
        "both halves of the origin come back: {wire}"
    );

    // A board-created dispatch adds nothing to the log, which is what keeps
    // every stored record from needing a migration.
    let from_the_board = CompanyEvent::TaskDispatched {
        task_id: "t-43".into(),
        run_id: None,
        origin_chat_id: None,
        origin_parent: None,
    };
    let bare = serde_json::to_string(&from_the_board).expect("serializes");
    assert!(
        !bare.contains("origin_chat_id") && !bare.contains("origin_parent"),
        "an absent origin is skipped, not written as null: {bare}"
    );

    // And a line from before the fields existed still replays, reading as the
    // board-created case rather than failing to decode.
    let old = r#"{"kind":"TaskDispatched","task_id":"t-44"}"#;
    assert_eq!(
        serde_json::from_str::<CompanyEvent>(old).expect("an older line replays"),
        CompanyEvent::TaskDispatched {
            task_id: "t-44".into(),
            run_id: None,
            origin_chat_id: None,
            origin_parent: None,
        },
    );
}

/// A dispatch raised from a thread says so, the way its completion already
/// does.
///
/// Without this the thread that asked went silent from the moment it
/// dispatched: the chat turn had genuinely succeeded — it handed the work over
/// — so its working row settled, and every frame after it named only a card.
/// The answer then arrived from nowhere minutes later, because
/// `desk_task_completed` *is* addressed to the conversation.
#[test]
fn a_dispatch_raised_in_a_thread_names_that_thread() {
    let v = super::project_event(&stored(CompanyEvent::TaskDispatched {
        task_id: "t-42".into(),
        run_id: Some("r-1".into()),
        origin_chat_id: Some("main".into()),
        origin_parent: Some(crate::ports::types::EventSeq::new(50)),
    }))
    .expect("task_dispatched is an attention signal");

    assert_eq!(v["chatId"], "main", "the conversation that asked");
    assert_eq!(
        v["parentId"], "50",
        "and the thread within it, as a string like every other parent here"
    );
}

/// Issue #464: an opened card reaches the console as its own frame. This is
/// the half a unit test can prove — that the projection exists and carries
/// the card; that the *board* redraws off it is a browser fact.
#[test]
fn projects_task_card_changed() {
    let v = super::project_event(&stored(CompanyEvent::TaskCardChanged {
        task_id: "t-77".into(),
        change: crate::runtime::CHANGE_OPENED.into(),
        column: Some("todo".into()),
    }))
    .expect("a board write is an attention signal");
    assert_eq!(v["type"], "task_card_changed");
    assert_eq!(v["taskId"], "t-77");
    assert_eq!(v["change"], "opened");
    assert_eq!(v["column"], "todo");
}

/// A removed card is projected without a column — the console's "is it
/// gone?" check is a presence check, never a null one.
#[test]
fn projects_a_removed_card_without_a_column() {
    let v = super::project_event(&stored(CompanyEvent::TaskCardChanged {
        task_id: "t-77".into(),
        change: crate::runtime::CHANGE_REMOVED.into(),
        column: None,
    }))
    .expect("a board write is an attention signal");
    assert_eq!(v["change"], "removed");
    assert!(
        v.get("column").is_none(),
        "a removed card is in no column: {v}"
    );
}

/// Issue #327: the workspace's own frame. The stream is deny-by-default, so
/// an event with no arm is silently unprojected — this is what proves the
/// arm exists at all.
///
/// Also pins what is **not** on the wire: no node name, no body. A note's
/// text is operator- or agent-authored free text, and this frame's job is
/// to say something moved, not to carry the tree.
#[test]
fn projects_workspace_changed_without_a_name_or_a_body() {
    let v = super::project_event(&stored(CompanyEvent::WorkspaceChanged {
        node_id: "n-9".into(),
        change: crate::runtime::CHANGE_UPDATED.into(),
    }))
    .expect("a workspace write must reach the console");
    assert_eq!(v["type"], "workspace_changed");
    assert_eq!(v["nodeId"], "n-9");
    assert_eq!(v["change"], "updated");
    assert!(v.get("name").is_none(), "no node name on the wire: {v}");
    assert!(v.get("content").is_none(), "no body on the wire: {v}");
}

#[test]
fn projects_mcp_call_failed_with_scrubbed_message() {
    let v = super::project_event(&stored(CompanyEvent::McpCallFailed {
        task_id: None,
        server: "browserbase".into(),
        tool: "browse".into(),
        status: "tool_call_rejected".into(),
        message: "server rejected the call".into(),
    }))
    .expect("mcp_call_failed is an attention signal");
    assert_eq!(v["type"], "mcp_call_failed");
    assert_eq!(v["server"], "browserbase");
    assert_eq!(v["tool"], "browse");
    assert_eq!(v["status"], "tool_call_rejected");
    // The message is already scrubbed at the source; we forward exactly it.
    assert_eq!(v["message"], "server rejected the call");
}

#[test]
fn projects_approval_resolved_without_the_actor() {
    let v = super::project_event(&stored(CompanyEvent::ApprovalResolved {
        approval_id: ApprovalId::new("ap-1"),
        verdict: Verdict::Approve,
        by: Actor {
            kind: ActorKind::User,
            // A user id must never reach the wire via the attention feed.
            id: "secret-user-id".into(),
        },
    }))
    .expect("approval_resolved is an attention signal");
    assert_eq!(v["type"], "approval_resolved");
    assert_eq!(v["approvalId"], "ap-1");
    assert_eq!(v["verdict"], "approve");
    // The actor is intentionally dropped — the projection carries no `by`,
    // and the serialized bytes never mention the user id.
    assert!(v.get("by").is_none(), "actor must not be projected");
    assert!(
        !v.to_string().contains("secret-user-id"),
        "user id leaked onto the wire"
    );
    // Issue #971: and a person's decision carries no `automatic` flag, so
    // the console's "an operator decided this" reading of its absence is
    // the correct one.
    assert!(
        v.get("automatic").is_none(),
        "a user's own decision is not automatic"
    );
}

/// **T6 (issue #971).** A host-side expiry says so, without saying who.
///
/// The defect: an expiry appends `ApprovalResolved { Deny, System }`, this
/// frame dropped the actor, and the console toasted "Approval denied" — so
/// an operator was told they had declined a request they never saw. With a
/// 24-hour deadline that stops being rare.
///
/// The assertion above is **extended here, not replaced**: the new field is
/// a bit derived from `by.kind`, and the no-actor / no-user-id property it
/// is derived from has to keep holding, so it is re-asserted on this arm
/// with a `System` actor whose id is equally secret.
#[test]
fn projects_a_host_side_expiry_as_automatic_without_the_actor() {
    let v = super::project_event(&stored(CompanyEvent::ApprovalResolved {
        approval_id: ApprovalId::new("ap-2"),
        verdict: Verdict::Deny,
        by: Actor {
            kind: ActorKind::System,
            // Even the system actor's id stays off the feed: the console
            // needs the *fact* that no person decided this, not the name of
            // the internal path that did.
            id: "expiry".into(),
        },
    }))
    .expect("approval_resolved is an attention signal");
    assert_eq!(v["type"], "approval_resolved");
    assert_eq!(v["approvalId"], "ap-2");
    assert_eq!(v["verdict"], "deny");
    assert_eq!(
        v["automatic"], true,
        "the console must be able to say the deadline passed rather than \
         attributing the deny to whoever is looking at it"
    );
    // The extended property, restated on this arm.
    assert!(v.get("by").is_none(), "actor must not be projected");
    assert!(
        !v.to_string().contains("expiry"),
        "the actor id must not reach the wire on this arm either"
    );
}

#[test]
fn projects_task_steered_without_actor_or_instruction() {
    let v = super::project_event(&stored(CompanyEvent::TaskSteered {
        task_id: "t-9".into(),
        action: "redirect".into(),
        instruction: Some("focus on the API".into()),
        by: Some(Actor {
            kind: ActorKind::User,
            id: "secret-user-id".into(),
        }),
    }))
    .expect("task_steered is an attention signal");
    assert_eq!(v["type"], "task_steered");
    assert_eq!(v["taskId"], "t-9");
    assert_eq!(v["action"], "redirect");
    let wire = v.to_string();
    assert!(!wire.contains("secret-user-id"));
    assert!(!wire.contains("focus on the API"));
}

#[test]
fn projects_workflow_created_without_the_actor() {
    let v = super::project_event(&stored(CompanyEvent::WorkflowCreated {
        workflow_id: "greeter".into(),
        name: "Greeter".into(),
        by: Some(Actor {
            kind: ActorKind::User,
            id: "secret-user-id".into(),
        }),
    }))
    .expect("workflow_created is an attention signal");
    assert_eq!(v["type"], "workflow_created");
    assert_eq!(v["workflowId"], "greeter");
    assert_eq!(v["name"], "Greeter");
    assert!(!v.to_string().contains("secret-user-id"));
}

/// Issue #259: the edit and delete signals project the same two fields and
/// drop the actor, exactly like `workflow_created` above.
#[test]
fn projects_workflow_updated_and_deleted_without_the_actor() {
    let actor = || {
        Some(Actor {
            kind: ActorKind::User,
            id: "secret-user-id".into(),
        })
    };

    let v = super::project_event(&stored(CompanyEvent::WorkflowUpdated {
        workflow_id: "greeter".into(),
        name: "Greeter v2".into(),
        by: actor(),
    }))
    .expect("workflow_updated is an attention signal");
    assert_eq!(v["type"], "workflow_updated");
    assert_eq!(v["workflowId"], "greeter");
    assert_eq!(v["name"], "Greeter v2");
    assert!(!v.to_string().contains("secret-user-id"));

    let v = super::project_event(&stored(CompanyEvent::WorkflowDeleted {
        workflow_id: "greeter".into(),
        name: "Greeter".into(),
        by: actor(),
    }))
    .expect("workflow_deleted is an attention signal");
    assert_eq!(v["type"], "workflow_deleted");
    assert_eq!(v["workflowId"], "greeter");
    assert_eq!(v["name"], "Greeter");
    assert!(!v.to_string().contains("secret-user-id"));
}

#[test]
fn projects_lifecycle_changed_without_the_actor() {
    let v = super::project_event(&stored(CompanyEvent::LifecycleChanged {
        from: "running".into(),
        to: "paused".into(),
        by: Actor {
            kind: ActorKind::Operator,
            id: "operator".into(),
        },
    }))
    .expect("lifecycle_changed is an attention signal");
    assert_eq!(v["type"], "lifecycle_changed");
    assert_eq!(v["from"], "running");
    assert_eq!(v["to"], "paused");
    assert!(v.get("by").is_none(), "actor must not be projected");
}

#[test]
fn projects_payment_received() {
    let v = super::project_event(&stored(CompanyEvent::PaymentReceived {
        amount_usd: 25.0,
        memo: "invoice #1".into(),
    }))
    .expect("payment_received is an attention signal");
    assert_eq!(v["type"], "payment_received");
    assert_eq!(v["amountUsd"], 25.0);
    assert_eq!(v["memo"], "invoice #1");
}

/// The live half of #228: a finished run reaches the console as it happens,
/// carrying exactly the fields the run drawer already renders — so the
/// console can toast an undelivered report instead of waiting for a reload.
#[test]
fn projects_workflow_run_finished_with_the_fields_the_drawer_renders() {
    let v = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
        workflow_id: "digest".into(),
        scheduled: true,
        run_id: None,
        deliveries: vec![
            delivery_row("owner_summary", crate::ports::DeliveryStatus::Skipped),
            delivery_row("also_sent", crate::ports::DeliveryStatus::Sent),
        ],
        pending_approvals: vec!["review".into()],
        error: None,
        cancelled: false,
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }))
    .expect("workflow_run_finished is an attention signal");
    assert_eq!(v["type"], "workflow_run_finished");
    assert_eq!(v["seq"], 7);
    assert_eq!(v["workflowId"], "digest");
    assert_eq!(v["scheduled"], true);
    assert_eq!(v["pendingApprovals"][0], "review");

    // Per-row node/kind/target/status/detail — the same shape the manual
    // run's HTTP response already ships to this console.
    let rows = v["deliveries"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["node"], "owner_summary");
    assert_eq!(rows[0]["kind"], "email");
    assert_eq!(rows[0]["status"], "skipped");
    assert_eq!(rows[0]["target"], "ada@example.com");
    assert!(
        rows[0]["detail"]
            .as_str()
            .unwrap()
            .contains("never written"),
        "the detail names the fix: {v}"
    );

    // A run that finished carries no `error` key, and `runId` — always
    // `None` today — is never a permanently-null key on the wire.
    assert!(v.get("error").is_none(), "{v}");
    assert!(v.get("runId").is_none(), "{v}");
}

/// The failure arm reaches the console too — it is the outcome that used to
/// produce nothing but a host-stdout warning.
#[test]
fn projects_workflow_run_finished_with_the_failure_reason() {
    let v = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
        workflow_id: "digest".into(),
        scheduled: true,
        run_id: None,
        deliveries: Vec::new(),
        pending_approvals: Vec::new(),
        error: Some("no inference source for agent node `worker`".into()),
        cancelled: false,
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }))
    .expect("workflow_run_finished is an attention signal");
    assert_eq!(v["error"], "no inference source for agent node `worker`");
    assert_eq!(v["deliveries"].as_array().unwrap().len(), 0);
}

/// Issues #881 / #880: the blocked arm reaches the console live.
///
/// Without it a console watching a run settle would be told it finished
/// cleanly — no error, not cancelled, nothing delivered — and then the
/// history it reloads a moment later would say the run blocked. The two
/// surfaces read the same journal event, so they must project the same
/// facts.
#[test]
fn projects_workflow_run_finished_with_its_blocked_nodes_and_parked_approvals() {
    let v = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
        workflow_id: "digest".into(),
        scheduled: true,
        run_id: Some("run-b".into()),
        deliveries: Vec::new(),
        pending_approvals: vec!["spec".into()],
        error: None,
        cancelled: false,
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: vec![crate::ports::WorkflowBlockedNode {
            node_id: "spec".into(),
            tools: vec!["publish_artifact".into()],
            approval_ids: vec!["appr-1".into()],
            unparkable: 0,
            stranded: 0,
            blockers: 0,
        }],
        approvals: vec![crate::ports::WorkflowRunApprovalRow {
            node_id: Some("spec".into()),
            tool: Some("publish_artifact".into()),
            outcome: crate::ports::WorkflowApprovalOutcome::Parked,
            approval_id: Some("appr-1".into()),
        }],
    }))
    .expect("workflow_run_finished is an attention signal");
    assert_eq!(v["blockedNodes"][0]["nodeId"], "spec");
    assert_eq!(v["blockedNodes"][0]["tools"][0], "publish_artifact");
    assert_eq!(v["approvals"][0]["outcome"], "parked");
    assert!(
        v.get("error").is_none(),
        "a run waiting on a person did not fail: {v}"
    );

    // The presence-check discipline: a run that blocked on nobody sends
    // neither key, so an existing frame is byte-unchanged.
    let clean = super::project_event(&stored(CompanyEvent::WorkflowRunFinished {
        workflow_id: "digest".into(),
        scheduled: true,
        run_id: None,
        deliveries: Vec::new(),
        pending_approvals: Vec::new(),
        error: None,
        cancelled: false,
        notices: Vec::new(),
        board: Vec::new(),
        blocked_nodes: Vec::new(),
        approvals: Vec::new(),
    }))
    .expect("projects");
    assert!(clean.get("blockedNodes").is_none(), "{clean}");
    assert!(clean.get("approvals").is_none(), "{clean}");
}

/// Issue #371: the live per-node trail. Both arms project, both carry the
/// run id that ties them to the run's settle-frame, and — the point — the
/// node arm carries a status and a duration and nothing else.
#[test]
fn projects_the_per_node_progress_trail() {
    let started = super::project_event(&stored(CompanyEvent::WorkflowRunStarted {
        workflow_id: "digest".into(),
        run_id: "run-1".into(),
        scheduled: true,
        started_by: None,
        resume_semantic: None,
    }))
    .expect("workflow_run_started reaches the console");
    assert_eq!(started["type"], "workflow_run_started");
    assert_eq!(started["workflowId"], "digest");
    assert_eq!(started["runId"], "run-1");
    assert_eq!(started["scheduled"], true);
    assert!(
        started.get("startedBy").is_none(),
        "no sender projects no key: {started}"
    );

    // Issue #1862 prerequisite: when the journal carries a sender, the SSE
    // frame forwards it under `startedBy`.
    let started_with_sender = super::project_event(&stored(CompanyEvent::WorkflowRunStarted {
        workflow_id: "digest".into(),
        run_id: "run-1".into(),
        scheduled: false,
        started_by: Some(crate::ports::types::StartedBy::Agent("ceo".into())),
        resume_semantic: None,
    }))
    .expect("workflow_run_started reaches the console");
    assert_eq!(
        started_with_sender["startedBy"],
        serde_json::json!({"agent": "ceo"})
    );

    let node = super::project_event(&stored(CompanyEvent::WorkflowNodeFinished {
        workflow_id: "digest".into(),
        run_id: "run-1".into(),
        node_id: "ceo".into(),
        status: crate::ports::types::WorkflowNodeStatus::Error,
        elapsed_ms: 1234,
        diagnostics: Vec::new(),
        agent_run_id: None,
    }))
    .expect("workflow_node_finished reaches the console");
    assert_eq!(node["type"], "workflow_node_finished");
    assert_eq!(node["runId"], "run-1");
    assert_eq!(node["nodeId"], "ceo");
    assert_eq!(node["status"], "error");
    assert_eq!(node["elapsedMs"], 1234);

    // The scrubbing claim, stated as a test: an errored node projects a
    // status word and NOTHING that could carry the node's own words. The
    // event type has no field to hold them, so this can only regress by
    // widening the event — which is the point of keeping it closed.
    let mut keys: Vec<&str> = node
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "atMillis",
            "elapsedMs",
            "nodeId",
            "runId",
            "seq",
            "status",
            "type",
            "workflowId",
        ],
        "the node frame carries only structural fields: {node}"
    );
}

/// Issue #382: the per-node START bracket reaches the console too. Without
/// its own arm it would fall to `project_event`'s `_ => return None` wildcard
/// and be silently dropped — the exact trap this file has been bitten by
/// three times — and the canvas would be back to guessing which node runs.
/// It carries the ids and NOTHING else: no status or duration (the node has
/// not run) and no input, so the frame is structural by construction.
#[test]
fn projects_the_per_node_started_bracket() {
    let node = super::project_event(&stored(CompanyEvent::WorkflowNodeStarted {
        workflow_id: "digest".into(),
        run_id: "run-1".into(),
        node_id: "ceo".into(),
    }))
    .expect("workflow_node_started reaches the console");
    assert_eq!(node["type"], "workflow_node_started");
    assert_eq!(node["workflowId"], "digest");
    assert_eq!(node["runId"], "run-1");
    assert_eq!(node["nodeId"], "ceo");

    // Structural-only: ids plus the envelope, and no status/duration/payload
    // slot the finish frame has. Regresses only by widening the event.
    let mut keys: Vec<&str> = node
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["atMillis", "nodeId", "runId", "seq", "type", "workflowId"],
        "the started frame carries only structural ids: {node}"
    );
}
