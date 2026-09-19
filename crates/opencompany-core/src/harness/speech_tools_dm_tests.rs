use super::speech_tools_test_fixtures::*;
use super::*;
use std::sync::Mutex;

#[tokio::test]
async fn a_committed_dm_stages_one_bounded_recipient_turn_without_a_card() {
    let (context, _events, _dir) = context_with_overlay_teammates().await;
    let queue = crate::harness::orchestrator::DelegationQueue::default();
    let _claim = queue.claim();
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken, async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            DmTool(context.with_dispatch(queue.clone())).execute(serde_json::json!({
                "to": ["copy", "nova"],
                "message": "Which headline survived review?"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");

    let staged = queue.drain(crate::harness::orchestrator::MAX_DELEGATIONS_PER_TURN);
    assert_eq!(
        staged.len(),
        1,
        "one DM call may wake at most one recipient"
    );
    assert!(matches!(
        &staged[0],
        crate::harness::orchestrator::Delegation::ConversationDispatch {
            source,
            target,
            message,
            child_hop: 1,
            ..
        } if source == "designer"
            && target == "copy"
            && message == "Which headline survived review?"
    ));
}

/// `desk_dm` must resolve a display name to the roster's canonical id
/// before journaling: `dm` names the recipient's session with it
/// (`openhuman_session_key`), and `agent_channels` registers that session
/// under the canonical id, never the typed name. Left unresolved, the row
/// would be journaled under a session nothing reads (Codex P2).
#[tokio::test]
async fn a_dm_to_a_display_name_resolves_to_the_canonical_id() {
    let (context, events, _dir) = context_with_overlay_teammates().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("designer".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["Nova"],
                "message": "ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");
    let appended = events.0.lock().expect("lock");
    assert_eq!(appended.len(), 1, "{appended:?}");
    let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
        panic!("expected an AgentReply, got {:?}", appended[0]);
    };
    assert_eq!(
        chat_id, "dm:designer+nova",
        "the row must be journaled under the canonical id, not the typed display name"
    );
}

/// A display name two teammates share must be refused, not silently
/// resolved to whichever one the roster happens to list first — that would
/// journal a row under a session the operator never meant to address.
#[tokio::test]
async fn a_dm_to_an_ambiguous_display_name_is_refused() {
    let (context, events, _dir) = context_with_overlay_teammates().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken, async {
        crate::runtime::delegation::with_turn_conversation(
            Some("designer".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["Rivers"],
                "message": "ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(result.is_error, "{result:?}");
    let said = format!("{result:?}");
    assert!(
        said.contains("more than one") || said.contains("Rivers"),
        "the refusal should name the collision: {said}"
    );
    assert!(
        events.0.lock().expect("lock").is_empty(),
        "an ambiguous name must journal nothing"
    );
}

/// Codex P2: canonicalizing `to` can turn a survivor of the raw self-id
/// filter back into the caller's own id. `nova` addressing itself as
/// `"Nova"` passes the first filter (`"Nova" != "nova"`) and must still be
/// refused once resolution reveals it is the caller.
#[tokio::test]
async fn a_dm_to_your_own_display_name_is_refused() {
    let (mut context, events, _dir) = context_with_overlay_teammates().await;
    context.agent_id = "nova".to_string();
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken, async {
        crate::runtime::delegation::with_turn_conversation(
            Some("designer".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["Nova"],
                "message": "note to self"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(result.is_error, "{result:?}");
    assert!(
        events.0.lock().expect("lock").is_empty(),
        "a self-DM by display name must journal nothing"
    );
}

/// tinysweeper: an unreadable roster must refuse a DM, not silently skip
/// the check and journal a row to whatever the caller typed — the same
/// rule `resolve_desk` already applies to a channel name.
#[tokio::test]
async fn a_dm_is_refused_when_the_roster_cannot_be_read() {
    let (context, events, _dir) = context();
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken, async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["copy"],
                "message": "ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(result.is_error, "{result:?}");
    assert!(
        events.0.lock().expect("lock").is_empty(),
        "an unreadable roster must journal nothing"
    );
}

/// `desk_post` can name a channel this agent sits on, and the line lands
/// there rather than in the conversation the turn is in.
///
/// This is the capability a live run found missing: asked to say something
/// on another desk, the agent had no argument for it and answered the person
/// who asked instead — which reads as the message having been passed on.
#[tokio::test]
async fn a_post_can_name_another_channel_this_agent_sits_on() {
    let (context, events, _dir) = context_with_desks().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("designer".to_string()),
            PostTool(context).execute(serde_json::json!({
                "desk": "brand",
                "message": "moving the hero to the left rail"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");
    let appended = events.0.lock().expect("lock");
    let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
        panic!("expected an AgentReply, got {:?}", appended[0]);
    };
    assert_eq!(
        chat_id, "brand",
        "a named desk is where it goes; `designer` is the DM the turn was in"
    );
}

/// A desk this agent is not a member of is refused, and the refusal says
/// which channels it can reach.
///
/// Widening to it would put a line in front of a room with no record of who
/// let it in. Reaching another desk is a referral — a crossing the library
/// already models, with its own provenance chip and return path.
#[tokio::test]
async fn a_post_to_a_desk_this_agent_is_not_on_is_refused() {
    let (context, events, _dir) = context_with_desks().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("designer".to_string()),
            PostTool(context).execute(serde_json::json!({
                "desk": "platform",
                "message": "you should refactor this"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(result.is_error, "{result:?}");
    let said = format!("{result:?}");
    assert!(said.contains("do not sit on"), "{said}");
    assert!(
        said.contains("Brand"),
        "the refusal names what it can reach: {said}"
    );
    assert!(
        events.0.lock().expect("lock").is_empty(),
        "a refused post journals nothing"
    );
}

/// The contract text is the crate's, not this host's. It is the only place a
/// seat is told that text outside a tool call reaches nobody, so a host that
/// paraphrased it would be quietly rewriting the rule.
///
/// `desk_dm` is the one exception, and it is one because the rule itself
/// changed here — see [`the_dm_description_says_the_answer_comes_back`].
#[test]
fn the_descriptions_are_the_crates_own() {
    for name in SPEECH_TOOLS {
        if bare(name) == bare(DM_TOOL) {
            continue;
        }
        let ours = crate_description(name);
        // `crate_spec_name`, not `bare`: upstream renamed `close` to
        // `complete_episode` and this assertion is what caught the belt still
        // looking for the old spelling.
        let theirs = speech::tool_specs()
            .iter()
            .find(|spec| spec.name == super::crate_spec_name(name))
            .expect("every registered tool is one the crate names")
            .description;
        assert_eq!(ours, theirs, "{name} paraphrased the crate");
    }
}

/// `desk_dm` is an ASK on this host, and the crate's text says it is not.
///
/// TinyHiveMind describes `dm` as a way to "say one thing" that costs the
/// seat's one message for the turn, and states that the room learns an exchange
/// happened and not what it said. Read against "find out what this teammate
/// knows", that is a price with no return — and a seat holding a factual gap
/// correctly reached for `delegate_to_teammate` instead, the only tool on the
/// belt that promised an answer, at a board card per call.
///
/// Here the recipient's turn runs inline and their reply is the call's result,
/// so the crate's sentence is no longer true of this host and describes the one
/// behaviour that would stop the tool being used (#2368).
#[test]
fn the_dm_description_says_the_answer_comes_back() {
    let ours = crate_description(DM_TOOL);
    let theirs = speech::tool_specs()
        .iter()
        .find(|spec| spec.name == bare(DM_TOOL))
        .expect("the crate names this tool")
        .description;
    assert_ne!(ours, theirs, "the override must not silently fall back");
    assert!(
        ours.contains("reply") && ours.contains("result"),
        "a seat must be able to read that the answer comes back: {ours}"
    );
    assert!(
        !ours.contains("settle a disagreement"),
        "the crate's framing is what steered seats away from asking: {ours}"
    );
}

/// A post is a *request* to speak: it does not append, it is collected and
/// the reply path appends it — with the steps, the live frame and the
/// mentions a tool does not hold.
#[tokio::test]
async fn a_post_is_collected_rather_than_journaled() {
    let (context, events, _dir) = context();
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            PostTool(context).execute(serde_json::json!({ "message": "warmer" })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");
    assert_eq!(spoken.utterances(), vec!["warmer".to_string()]);
    assert!(spoken.spoke());
    assert!(
        events.0.lock().expect("lock").is_empty(),
        "a post must not append; the reply path does"
    );
}

/// A DM lands in the **recipient's** channel, not the speaker's.
///
/// This test exists because the first version of `desk_dm` did the opposite
/// and a live run caught it: the row went to the ambient channel with
/// `audience: [recipient]`, which put a message meant for a teammate into
/// the *operator's* DM with the speaker. `agent_channels` never gives the
/// recipient the speaker's own DM, so the one person named could not read
/// it and the one person not named could — and the tool said "Said."
///
/// So the assertions are about `chat_id`, and the empty `audience` is as
/// load-bearing as the id: a non-empty one would make `fold_asides` lift the
/// row out of the transcript as a desk deliberation aside, which it is not.
#[tokio::test]
async fn a_dm_lands_in_the_recipients_own_channel() {
    let (context, events, _dir) = context_with_overlay_teammates().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["copy"],
                "message": "between us: ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");
    let appended = events.0.lock().expect("lock");
    assert_eq!(appended.len(), 1, "{appended:?}");
    let CompanyEvent::AgentReply {
        audience,
        agent_id,
        chat_id,
        ..
    } = &appended[0]
    else {
        panic!("expected an AgentReply, got {:?}", appended[0]);
    };
    assert_eq!(agent_id, "designer");
    assert_eq!(
        chat_id, "dm:copy+designer",
        "a DM belongs in the PAIR's own thread (#2368). The recipient's own channel is the \
         one the operator uses to DM them, so agent-to-agent traffic landed in a human's \
         thread authored by somebody not talking to them — and the sender could not read it \
         back at all, because `agent_channels` gives an agent its own line and not its peers'"
    );
    assert!(
        audience.is_empty(),
        "the channel is the narrowing; an audience would make this a deliberation aside"
    );
}

/// Codex P1: when the recipient's bare agent id collides with a desk id,
/// `chat_history::owns` would let every member of that desk read the
/// row, so the DM must land under the `dm:`-prefixed spelling instead —
/// the same spelling `agent_channels` already registers as this
/// teammate's own line.
#[tokio::test]
async fn a_dm_to_a_peer_whose_id_collides_with_a_desk_lands_under_the_prefixed_key() {
    let (context, events, _dir) = context();
    let manifest: crate::company::CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "platform"
role = "Platform engineer"
description = "Builds things."

[[group_chat]]
id = "platform"
name = "Platform"
members = ["designer"]
"#,
    )
    .expect("valid manifest");
    let record = crate::ports::types::CompanyRecord {
        id: context.company.clone(),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_tool_grants: None,
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
    };
    context.store.save(&record).await.expect("the record saves");
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("platform".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["platform"],
                "message": "between us: ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");
    let appended = events.0.lock().expect("lock");
    assert_eq!(appended.len(), 1, "{appended:?}");
    let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
        panic!("expected an AgentReply, got {:?}", appended[0]);
    };
    assert_eq!(
        chat_id, "dm:designer+platform",
        "the bare id collides with the `platform` desk, so a bare-keyed row would be \
         readable by every member of it. The pair key is collision-free by construction — it \
         names BOTH parties — which is what retired the prefixing this case used to need"
    );
}

/// Codex P1 (fresh evidence): a desk's *display name*, not just its id,
/// collides the same way — `chat_history::owns` matches on either — so a
/// desk `{ id = "triage", name = "support" }` makes a DM to agent
/// `support` exactly as readable-by-the-whole-desk as an id collision
/// would.
#[tokio::test]
async fn a_dm_to_a_peer_whose_id_collides_with_a_desk_name_lands_under_the_prefixed_key() {
    let (context, events, _dir) = context();
    let manifest: crate::company::CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "support"
role = "Support"
description = "Answers things."

[[group_chat]]
id = "triage"
name = "support"
members = ["designer"]
"#,
    )
    .expect("valid manifest");
    let record = crate::ports::types::CompanyRecord {
        id: context.company.clone(),
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_tool_grants: None,
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
    };
    context.store.save(&record).await.expect("the record saves");
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("triage".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["support"],
                "message": "between us: ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");
    let appended = events.0.lock().expect("lock");
    assert_eq!(appended.len(), 1, "{appended:?}");
    let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
        panic!("expected an AgentReply, got {:?}", appended[0]);
    };
    assert_eq!(
        chat_id, "dm:designer+support",
        "the bare id collides with the `triage` desk's display NAME (\"support\"), \
         which `owns` matches exactly like an id collision. Naming both parties \
         sidesteps the whole class: a pair key can only collide with the same \
         pair's own thread"
    );
}

/// Two recipients get two rows, one in each of their channels.
///
/// Not one row with both in an audience: there is no channel both of them
/// read, and inventing one would be inventing a group nobody created.
#[tokio::test]
async fn a_dm_to_two_teammates_leaves_a_row_in_each_of_their_channels() {
    let (context, events, _dir) = context_with_overlay_teammates().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["copy", "researcher"],
                "message": "both of you: ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");
    let appended = events.0.lock().expect("lock");
    let channels: Vec<&str> = appended
        .iter()
        .map(|event| match event {
            CompanyEvent::AgentReply { chat_id, .. } => chat_id.as_str(),
            other => panic!("expected an AgentReply, got {other:?}"),
        })
        .collect();
    assert_eq!(
        channels,
        vec!["dm:copy+designer", "dm:designer+researcher"],
        "one row per pair thread, and `pair_conversation` sorts the ids so the same two \
         teammates share ONE thread whichever of them speaks"
    );
}

/// Codex P2: `to` naming the same recipient twice — by a repeated raw id,
/// or (elsewhere) once by id and once by a display name that resolves to
/// the same canonical id — must not double the row in their channel.
#[tokio::test]
async fn a_repeated_recipient_leaves_only_one_row() {
    let (context, events, _dir) = context_with_overlay_teammates().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["copy", "copy"],
                "message": "said once, meant once"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(!result.is_error, "{result:?}");
    let appended = events.0.lock().expect("lock");
    assert_eq!(
        appended.len(),
        1,
        "a repeated `to` entry must not journal a second row: {appended:?}"
    );
}

/// coderabbit: a journal failure for one recipient must not read as a
/// failure for all of them — the recipients ahead of the failing one
/// already have a durable row, and reporting a flat error invites a retry
/// that journals a second one for each. `researcher`'s append is made to
/// fail; `copy`'s must still land, and the result must say so by name
/// rather than simply being `is_error`.
#[tokio::test]
async fn a_dm_failure_for_one_recipient_does_not_undo_or_hide_an_earlier_success() {
    let dir = tempfile::Builder::new()
        .prefix("speech-tools-")
        .tempdir()
        .expect("tempdir");
    let store: Arc<dyn crate::ports::store::CompanyStore> =
        Arc::new(crate::store::FsCompanyStore::new(dir.path()));
    let company = CompanyId::new("acme");
    let events = Arc::new(FlakyLog {
        events: Mutex::new(Vec::new()),
        // The PAIR thread, not the bare id — that is the key a `desk_dm` writes
        // under since #2368, and a fixture refusing the old one refuses nothing.
        refuses: "dm:designer+researcher",
    });
    let context = SpeechContext::new(
        company.clone(),
        "designer".to_string(),
        events.clone() as Arc<dyn EventLog>,
        store.clone(),
    );
    let manifest: crate::company::CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "designer"
role = "Designer"
description = "Draws things."

[[agent]]
id = "copy"
role = "Copywriter"
description = "Writes things."

[[agent]]
id = "researcher"
role = "Researcher"
description = "Finds things."

[[group_chat]]
id = "brand"
name = "Brand"
members = ["designer", "copy", "researcher"]
"#,
    )
    .expect("valid manifest");
    let record = crate::ports::types::CompanyRecord {
        id: company,
        manifest,
        ledger: Vec::new(),
        lifecycle: "running".to_string(),
        setup: None,
        name_confirmed: false,
        activation_completed_at: None,
        created_at_millis: None,
        overlay_agents: Vec::new(),
        overlay_desk_members: Vec::new(),
        overlay_desk_order: Vec::new(),
        overlay_desks: Vec::new(),
        overlay_desk_hive: Vec::new(),
        overlay_retired_agents: Vec::new(),
        overlay_agent_edits: Vec::new(),
        overlay_tool_grants: None,
        overlay_workflows: Vec::new(),
        overlay_budgets: Vec::new(),
        overlay_policy: None,
        overlay_desk_tools: Default::default(),
        disabled_workflows: Vec::new(),
        template_provenance: None,
    };
    store.save(&record).await.expect("the record saves");
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["copy", "researcher"],
                "message": "both of you: ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    let appended = events.events.lock().expect("lock");
    assert_eq!(
        appended.len(),
        1,
        "copy's row must still be journaled despite researcher's failure: {appended:?}"
    );
    let CompanyEvent::AgentReply { chat_id, .. } = &appended[0] else {
        panic!("expected an AgentReply, got {:?}", appended[0]);
    };
    assert_eq!(chat_id, "dm:copy+designer");
    let text = tool_result_text(&result);
    assert!(
        text.contains("copy"),
        "the reply must name who it did reach: {text}"
    );
    assert!(
        text.contains("researcher"),
        "the reply must name who it did not reach, so a retry can target just them: {text}"
    );
}

/// The result sentence does not claim delivery.
///
/// Nothing here wakes the recipient — `AgentReply::mentions` is never
/// consulted by dispatch, which is the mention-loop fuse — so the row waits
/// until their next turn. A live run showed why the wording matters: told
/// "Said. Journaled at [67]", the agent reported to the person who asked
/// that the message had been "passed to them directly (delivered)". An
/// agent repeats what its tools tell it.
#[tokio::test]
async fn a_dm_says_it_was_left_rather_than_delivered() {
    let (context, events, _dir) = context_with_overlay_teammates().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["copy"],
                "message": "ship it"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    let said = format!("{result:?}");
    assert!(said.contains("Left for @copy"), "{said}");
    assert!(said.contains("next turn"), "{said}");
    assert!(
        !said.to_lowercase().contains("delivered now\" "),
        "the sentence must not read as delivery: {said}"
    );
    drop(events);
}

/// Self-addressing is refused in three places in the crate and is refused
/// here too, at the one point that holds the speaker's own id. A row whose
/// audience is only its author is a covert channel with a journal entry.
#[tokio::test]
async fn a_dm_to_yourself_is_refused() {
    let (context, events, _dir) = context_with_overlay_teammates().await;
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken, async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            DmTool(context).execute(serde_json::json!({
                "to": ["designer"],
                "message": "note to self"
            })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(result.is_error, "{result:?}");
    assert!(events.0.lock().expect("lock").is_empty());
}

/// A turn with no conversation has no channel to speak into. Posting into a
/// guessed one would put a line in front of people who were not in the
/// exchange, so this refuses rather than defaulting.
#[tokio::test]
async fn speaking_outside_a_channel_is_refused() {
    let (context, events, _dir) = context();
    let result = PostTool(context)
        .execute(serde_json::json!({ "message": "anyone there?" }))
        .await
        .expect("the tool runs");
    assert!(result.is_error, "{result:?}");
    assert!(events.0.lock().expect("lock").is_empty());
}

/// An empty message is not silence, it is a mistake — and saying so is
/// better than journaling a blank row.
#[tokio::test]
async fn an_empty_message_is_refused() {
    let (context, _events, _dir) = context();
    let spoken = crate::runtime::delegation::new_turn_speech();
    let result = crate::runtime::delegation::with_turn_speech(spoken.clone(), async {
        crate::runtime::delegation::with_turn_conversation(
            Some("brand".to_string()),
            PostTool(context).execute(serde_json::json!({ "message": "   " })),
        )
        .await
    })
    .await
    .expect("the tool runs");
    assert!(result.is_error, "{result:?}");
    assert!(!spoken.spoke());
}

// -------------------------------------------------------------------
