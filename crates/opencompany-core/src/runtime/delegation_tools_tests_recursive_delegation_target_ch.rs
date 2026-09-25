use super::tests_core::*;

#[test]
fn manifest_entries_cover_both_delegation_tools() {
    let entries = delegation_manifest_entries();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&SPAWN_TASK_TOOL), "got {names:?}");
    assert!(names.contains(&DELEGATE_TO_DESK_TOOL), "got {names:?}");
    // Every entry carries a schema so Medulla can shape the call.
    assert!(
        entries
            .iter()
            .all(|e| e.input_schema.is_some() && e.description.is_some())
    );
}

#[test]
fn spawn_task_args_require_a_nonblank_title() {
    assert_eq!(SpawnTaskArgs::parse(&json!({})), None);
    assert_eq!(SpawnTaskArgs::parse(&json!({ "title": "  " })), None);
    let parsed = SpawnTaskArgs::parse(&json!({
        "title": "  Ship it ",
        "note": " brief ",
        "assignee": " eng "
    }))
    .expect("valid");
    assert_eq!(parsed.title, "Ship it");
    assert_eq!(parsed.note.as_deref(), Some("brief"));
    assert_eq!(parsed.assignee.as_deref(), Some("eng"));
    // Blank optionals collapse to None.
    let bare =
        SpawnTaskArgs::parse(&json!({ "title": "x", "note": "", "assignee": "" })).expect("valid");
    assert_eq!(bare.note, None);
    assert_eq!(bare.assignee, None);
}

/// Issue #1725: the four arms the harness brain used to hold inline. The
/// cycle's small-talk fast path answers in the same voice a turn would
/// have, and this is the resolution both now share.
#[test]
fn chat_responder_resolves_a_desk_a_teammate_and_a_dm_key() {
    let record = record();
    // A desk key -> its lead member.
    assert_eq!(
        chat_responder(&record, "engineering").as_deref(),
        Some("ceo")
    );
    // A desk *name*, case-insensitively, resolves the same way.
    assert_eq!(
        chat_responder(&record, "Content desk").as_deref(),
        Some("writer")
    );
    // A bare roster teammate id -> that teammate.
    assert_eq!(chat_responder(&record, "writer").as_deref(), Some("writer"));
    // The console's DM thread key, unwrapped (issue #982, step 3).
    assert_eq!(
        chat_responder(&record, "dm:writer").as_deref(),
        Some("writer")
    );
    assert_eq!(
        chat_responder(&record, "dm:engineering").as_deref(),
        Some("ceo")
    );
    // A desk whose members are all off the roster resolves to nobody, and
    // so does a key that names nothing — the caller decides the fallback.
    assert_eq!(chat_responder(&record, "legal"), None);
    assert_eq!(chat_responder(&record, "nope"), None);
    assert_eq!(chat_responder(&record, "dm:"), None);
}

/// The built-in `#general` channel resolves to **nobody**, so both callers
/// answer as their own orchestrator (issue #1743).
///
/// The teammate is the point: `mint_agent_id` reserves `main` and
/// `General`, but a manifest can declare one, and without the guard the
/// roster arm hands it every unaddressed message — while
/// `GET chat/history?desk=main` returns the folded General conversation
/// rather than that teammate's transcript. The bare key is the line; the
/// teammate keeps its DM.
#[test]
fn chat_responder_leaves_the_general_line_to_the_caller() {
    let mut record = record();
    for spelling in ["", "main", "Main", "MAIN", "general", "General"] {
        assert_eq!(
            chat_responder(&record, spelling),
            None,
            "the company's line resolves to nobody, addressed as {spelling:?}"
        );
    }

    record
        .overlay_agents
        .push(crate::ports::types::OverlayAgent {
            provider: None,
            id: "main".to_string(),
            name: "Mainard".to_string(),
            role: "Analyst".to_string(),
            description: None,
            tools: None,
            skills: None,
            model: None,
            harness: None,
        });
    assert!(record.is_roster_agent("main"), "the roster arm would match");
    assert_eq!(
        chat_responder(&record, "main"),
        None,
        "a teammate called `main` does not inherit the company's line"
    );
    assert_eq!(
        chat_responder(&record, "dm:main").as_deref(),
        Some("main"),
        "and keeps its own DM, which is how the console addresses one"
    );

    // An overlay desk squatting the key does not take it either — the
    // resolver declines it (see `resolve_desk_id`).
    record.overlay_desks.push(crate::ports::types::OverlayDesk {
        id: "main".to_string(),
        name: "Front office".to_string(),
        description: None,
        responder: Default::default(),
        members: vec!["writer".to_string()],
        hive: Default::default(),
    });
    assert_eq!(chat_responder(&record, "main"), None);

    // A desk the *blueprint* declares still wins, as it always has.
    let declared: crate::CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"writer\"\nrole = \"Writer\"\n\n[[group_chat]]\nid = \"main\"\nname = \"Front office\"\nmembers = [\"writer\"]\n",
    )
    .expect("valid manifest");
    record.manifest.group_chats.extend(declared.group_chats);
    assert_eq!(chat_responder(&record, "main").as_deref(), Some("writer"));
}

/// A `dm:` key names a teammate, even when a desk shares that id.
///
/// A blueprint may declare both — manifest validation does not forbid the
/// collision — and unwrapping the prefix into the desk-first resolver let
/// the desk's lead answer a DM addressed to the teammate. The prefixed
/// address exists precisely to reach that teammate, so it would have been
/// wrong in the one case it was added for.
#[test]
fn a_prefixed_dm_reaches_the_teammate_even_when_a_desk_shares_the_id() {
    let mut record = record();
    let declared: crate::CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"writer\"\nrole = \"Writer\"\n\n[[group_chat]]\nid = \"main\"\nname = \"Front office\"\nmembers = [\"writer\"]\n",
    )
    .expect("valid manifest");
    record.manifest.group_chats.extend(declared.group_chats);
    record
        .overlay_agents
        .push(crate::ports::types::OverlayAgent {
            provider: None,
            id: "main".to_string(),
            name: "Mainard".to_string(),
            role: "Analyst".to_string(),
            description: None,
            tools: None,
            skills: None,
            model: None,
            harness: None,
        });
    assert_eq!(
        chat_responder(&record, "dm:main").as_deref(),
        Some("main"),
        "the prefix names the teammate, not the desk that shares its id"
    );
    // The bare key still belongs to the desk, which is what claims the line.
    assert_eq!(
        chat_responder(&record, "main").as_deref(),
        Some("writer"),
        "the desk still answers its own id"
    );
}

/// ...and so does one that claims it by **id**, which is the other half.
///
/// A manifest may declare the General desk either way, and which half it
/// uses is arbitrary. Re-asking under a fixed `DEFAULT_DESK` ("General")
/// recognised only the display-name claimant, so for `id = "main", name =
/// "Front office"` a turn addressed `main` reached its lead while the
/// folded sibling `General` fell through to the orchestrator — one channel,
/// two responders, decided by whichever accepted alias the caller used.
#[test]
fn chat_responder_folds_the_general_aliases_to_an_id_claimant() {
    let mut record = record();
    let declared: crate::CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"writer\"\nrole = \"Writer\"\n\n[[group_chat]]\nid = \"main\"\nname = \"Front office\"\nmembers = [\"writer\"]\n",
    )
    .expect("valid manifest");
    record.manifest.group_chats.extend(declared.group_chats);

    let direct = chat_responder(&record, "main");
    assert!(direct.is_some(), "the desk answers under its own id");
    // Every folded spelling reaches the same lead — one channel, one voice.
    for alias in ["", "General", "general", "MAIN"] {
        assert_eq!(
            chat_responder(&record, alias),
            direct,
            "the folded alias {alias:?} must reach the same responder as `main`"
        );
    }
}

/// A desk that claims the line by **display name** answers every spelling
/// folded into it, not just the one it is spelled with (issue #1743).
///
/// `id = "ops", name = "General"` answers to `General` but not to `main`,
/// so asking for the raw key alone handed a `main` turn to the caller's
/// orchestrator while an `ops` turn went to that desk's lead — two voices
/// in the one channel the console renders for both. The General arm re-asks
/// under `DEFAULT_DESK`, which is the same fold `everyone_desk` applies, so
/// who answers and who `@everyone` names cannot disagree.
#[test]
fn chat_responder_folds_the_general_aliases_to_a_display_name_claimant() {
    let plain = record();
    let mut record = record();
    let declared: crate::CompanyManifest = toml::from_str(
        "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"writer\"\nrole = \"Writer\"\n\n[[group_chat]]\nid = \"ops\"\nname = \"General\"\nmembers = [\"writer\"]\n",
    )
    .expect("valid manifest");
    record.manifest.group_chats.extend(declared.group_chats);

    for spelling in ["", "main", "Main", "general", "General", "ops"] {
        assert_eq!(
            chat_responder(&record, spelling).as_deref(),
            Some("writer"),
            "one voice in the channel, addressed as {spelling:?}"
        );
    }
    // The other desks are untouched, and a company with no claimant still
    // leaves the line to the caller.
    assert_eq!(
        chat_responder(&record, "engineering").as_deref(),
        Some("ceo")
    );
    assert_eq!(chat_responder(&plain, "main"), None);
}

#[test]
fn desk_ids_lists_manifest_desks_in_declaration_order() {
    assert_eq!(desk_ids(&record()), ["engineering", "content", "legal"]);
}

/// What this function lists and what [`CompanyRecord::resolve_desk_id`]
/// resolves must be the same set (issue #1743).
///
/// `create_desk` accepted the General spellings until that issue, so an
/// upgraded record can hold an overlay desk called `main` or `general` —
/// and `resolve_desk_id` now declines to match an overlay desk against one,
/// because such a desk would otherwise answer for the built-in `#general`
/// channel. Grounding the model on an id every `delegate_to_desk` call is
/// then refused for is a loop the model cannot get out of: the refusal
/// names the desk set, the desk set names the target, the target is
/// refused.
#[test]
fn desk_ids_omits_an_overlay_desk_no_key_can_resolve() {
    let mut record = record();
    record.overlay_desks.push(crate::ports::types::OverlayDesk {
        id: "main".to_string(),
        name: "Front office".to_string(),
        description: None,
        responder: Default::default(),
        members: vec!["ceo".to_string()],
        hive: Default::default(),
    });
    // Named `General`, but addressable under its own id — it stays.
    record.overlay_desks.push(crate::ports::types::OverlayDesk {
        id: "ops".to_string(),
        name: "General".to_string(),
        description: None,
        responder: Default::default(),
        members: vec!["writer".to_string()],
        hive: Default::default(),
    });

    assert_eq!(
        desk_ids(&record),
        ["engineering", "content", "legal", "ops"],
        "the desk no key resolves is not offered as a target"
    );
    // The property, stated directly: every id listed resolves.
    for id in desk_ids(&record) {
        assert!(
            record.resolve_desk_id(&id).is_some(),
            "desk_ids offered {id:?}, which resolve_desk_id refuses"
        );
    }
    assert!(
        record.resolve_desk_id("main").is_none(),
        "and the omitted one is omitted because it does not resolve"
    );
}

/// A `[[group_chat]]` the **blueprint** declares under a General spelling
/// is grandfathered and stays listed — the rule above is about overlay
/// desks only, and narrowing it further would take a delegation target away
/// from a company whose manifest has always had one.
#[test]
fn desk_ids_keeps_a_blueprint_desk_declared_under_a_general_spelling() {
    let mut record = record();
    let declared: crate::CompanyManifest = toml::from_str(
        r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[group_chat]]
id = "main"
name = "Front office"
members = ["ceo"]
"#,
    )
    .expect("valid manifest");
    record.manifest.group_chats.extend(declared.group_chats);
    assert!(desk_ids(&record).contains(&"main".to_string()));
    assert_eq!(record.resolve_desk_id("main"), Some("main".to_string()));
}

#[test]
fn a_real_desk_with_a_lead_is_not_rejected() {
    assert_eq!(reject_desk_target(&record(), "content"), None);
    // The id-or-name key `resolve_desk_id` already accepts still works.
    assert_eq!(reject_desk_target(&record(), "Content desk"), None);
}

/// The observed bug: `writer` is a teammate, not a desk. The refusal must
/// name the real desk ids so the model can retry in the same turn, and say
/// which desk that teammate is actually on.
#[test]
fn an_invented_desk_is_rejected_with_the_valid_set() {
    let message = reject_desk_target(&record(), "writer").expect("rejected");
    assert!(message.contains("engineering"), "{message}");
    assert!(message.contains("content"), "{message}");
    assert!(message.contains("legal"), "{message}");
    assert!(
        message.contains("teammate"),
        "a teammate-as-desk target must be named as such: {message}"
    );
    // A key that is neither a desk nor a teammate still gets the valid set.
    let message = reject_desk_target(&record(), "growth").expect("rejected");
    assert!(message.contains("engineering"), "{message}");
    assert!(!message.contains("teammate"), "{message}");
}

/// A desk that exists but has nobody on the roster can never run a turn, so
/// it is refused too — with the desks that CAN take work.
#[test]
fn a_desk_with_no_roster_lead_is_rejected() {
    let message = reject_desk_target(&record(), "legal").expect("rejected");
    assert!(message.contains("no member on the roster"), "{message}");
    assert!(
        message.contains("Desks that can take work: engineering, content."),
        "only desks with a lead may be offered as alternatives: {message}"
    );
}

/// A company with no desks at all cannot delegate; say so rather than
/// offering an empty list.
#[test]
fn a_company_with_no_desks_says_so() {
    let mut record = record();
    record.manifest.group_chats.clear();
    let message = reject_desk_target(&record, "content").expect("rejected");
    assert!(message.contains("no desks at all"), "{message}");
}

#[test]
fn the_desk_list_is_capped_and_counts_the_remainder() {
    let ids: Vec<String> = (0..LISTED_DESKS + 3).map(|i| format!("d{i}")).collect();
    let list = desk_list(ids).expect("non-empty");
    assert!(list.ends_with("(+3 more)"), "{list}");
    assert_eq!(desk_list(Vec::new()), None);
}

// --- Recursive-delegation target checks (issue #176) -------------------

/// The A→B→A loop the issue names: a desk already on the chain cannot be
/// handed the work back.
#[test]
fn a_desk_already_on_the_chain_is_refused_as_a_cycle() {
    let record = record();
    let chain = vec!["engineering".to_string()];
    let message = reject_cycle_target(&record, &chain, "engineering", "writer").expect("rejected");
    assert!(message.contains("engineering"), "{message}");
    assert!(message.contains("loop"), "{message}");
    // A desk that is NOT on the chain is a step forward.
    assert_eq!(reject_cycle_target(&record, &chain, "content", "ceo"), None);
    // The id-or-name key `resolve_desk_id` accepts is checked identically —
    // a cycle written with the display name is still a cycle.
    assert!(reject_cycle_target(&record, &chain, "Engineering desk", "writer").is_some());
}

/// Handing work to the desk you lead is handing it to yourself.
#[test]
fn a_desk_the_caller_leads_is_refused_as_self_delegation() {
    let record = record();
    // `writer` leads `content`.
    let message = reject_cycle_target(&record, &[], "content", "writer").expect("rejected");
    assert!(message.contains("yourself"), "{message}");
    // Somebody who does NOT lead it may hand work to it.
    assert_eq!(reject_cycle_target(&record, &[], "content", "ceo"), None);
}

/// D1 itself: the lead of a three-person desk may hand a slice to either
/// peer on it, with no allowlist and no orchestrator round-trip — and, with
/// no allowlist, to everybody else on the roster too, desk-mates first.
#[test]
fn a_desk_lead_may_hand_work_to_a_peer_on_its_own_desk() {
    let record = desk_record();
    assert_eq!(
        teammate_targets(&record, "brand_strategist", &[]),
        ["seo_specialist", "copywriter", "chief", "analyst"],
        "own-desk peers first, in the desk's own order, then the rest of the roster, never \
         the caller"
    );
    // A list that names desks narrows the same call to desk-mates plus the
    // named desks' members.
    assert_eq!(
        teammate_targets(&record, "brand_strategist", &["strategy".to_string()]),
        ["seo_specialist", "copywriter"],
    );
    assert_eq!(
        reject_teammate_target(&record, Some("brand_strategist"), &[], "seo_specialist"),
        None
    );
    // The key is resolved, not matched: a typed capital is the same person.
    assert_eq!(
        reject_teammate_target(&record, Some("brand_strategist"), &[], "SEO_Specialist"),
        None
    );
}

/// A teammate on neither the caller's desks nor an allowlisted one is
/// refused — and the refusal names who the caller CAN reach, because the
/// model has no other way to learn its own closed set.
#[test]
fn a_teammate_out_of_reach_is_refused_with_the_reachable_set() {
    let record = desk_record();
    let narrowed = vec!["strategy".to_string()];
    let message = reject_teammate_target(&record, Some("brand_strategist"), &narrowed, "analyst")
        .expect("rejected");
    assert!(message.contains("analyst"), "{message}");
    assert!(message.contains("seo_specialist"), "{message}");
    assert!(message.contains(DELEGATE_TO_TEAMMATE_TOOL), "{message}");

    // …and the #176 allowlist admits that desk's members, at teammate
    // granularity: permission to hand work to a desk is permission to hand
    // it to somebody on it.
    let allowed = vec!["data".to_string()];
    assert_eq!(
        reject_teammate_target(&record, Some("brand_strategist"), &allowed, "analyst"),
        None
    );
    // `"*"` and an empty list both admit everybody — `analyst` is on `data`.
    assert_eq!(
        reject_teammate_target(&record, Some("brand_strategist"), &["*".into()], "analyst"),
        None
    );
    assert_eq!(
        reject_teammate_target(&record, Some("brand_strategist"), &[], "analyst"),
        None
    );
}

/// An unrestricted reach — an empty list or the wildcard — is the **whole
/// roster**, the orchestrator included, not only the members of desks: a
/// specialist with a question only the orchestrator can settle must be able
/// to put it to them, and nobody is written a list by default.
#[test]
fn an_unrestricted_teammate_reach_is_the_whole_roster() {
    let record = desk_record();
    for allowed in [Vec::new(), vec!["*".to_string()]] {
        let targets = teammate_targets(&record, "brand_strategist", &allowed);
        assert_eq!(
            targets,
            ["seo_specialist", "copywriter", "chief", "analyst"],
            "desk-mates first, then the rest of the roster: {targets:?}"
        );
        assert_eq!(
            reject_teammate_target(&record, Some("brand_strategist"), &allowed, "chief"),
            None
        );
    }
    assert!(reach_is_unrestricted(&[]));
    assert!(reach_is_unrestricted(&["*".to_string()]));
    assert!(!reach_is_unrestricted(&["strategy".to_string()]));
}

/// Handing work to yourself re-enters the turn already running.
#[test]
fn a_teammate_hand_off_to_yourself_is_refused() {
    let record = desk_record();
    let message = reject_teammate_target(&record, Some("seo_specialist"), &[], "seo_specialist")
        .expect("rejected");
    assert!(message.contains("yourself"), "{message}");
}

/// A key that is nobody is refused, and a key that names a **desk** is
/// redirected at the tool that takes desks — the mirror of the
/// teammate-as-desk arm `delegate_to_desk` already has.
#[test]
fn an_unknown_teammate_target_is_grounded() {
    let record = desk_record();
    let message =
        reject_teammate_target(&record, Some("brand_strategist"), &[], "nobody").expect("rejected");
    assert!(message.contains("no \"nobody\""), "{message}");
    assert!(message.contains("copywriter"), "{message}");

    let message =
        reject_teammate_target(&record, Some("brand_strategist"), &[], "data").expect("rejected");
    assert!(message.contains(DELEGATE_TO_DESK_TOOL), "{message}");
    assert!(message.contains("is a desk, not a teammate"), "{message}");
}

/// The orchestrator's copy is unrestricted — every roster teammate, no
/// allowlist — exactly as its `delegate_to_desk` is. Grounding still applies.
#[test]
fn the_orchestrators_teammate_target_set_is_the_whole_roster() {
    let record = desk_record();
    for target in ["analyst", "seo_specialist", "copywriter"] {
        assert_eq!(reject_teammate_target(&record, None, &[], target), None);
    }
    let message = reject_teammate_target(&record, None, &[], "nobody").expect("rejected");
    assert!(
        message.contains("analyst"),
        "the roster is listed: {message}"
    );
    assert_eq!(
        roster_agent_ids(&record),
        [
            "chief",
            "brand_strategist",
            "seo_specialist",
            "copywriter",
            "analyst"
        ]
    );
}

/// A→B→A: `seo_specialist` cannot hand the work back to the
/// `brand_strategist` whose turn it is running inside.
#[test]
fn a_teammate_already_on_the_chain_is_refused_as_a_cycle() {
    let record = desk_record();
    let chain = vec![teammate_scope_key("brand_strategist")];
    let message =
        reject_teammate_cycle_target(&record, &chain, "brand_strategist").expect("rejected");
    assert!(message.contains("loop"), "{message}");
    assert!(
        message.contains("brand_strategist") && !message.contains("agent:"),
        "the trail must read as names, not as the chain's encoding: {message}"
    );
    // A different peer is a step forward.
    assert_eq!(
        reject_teammate_cycle_target(&record, &chain, "copywriter"),
        None
    );
    // Case is folded on both sides — a cycle spelled differently is a cycle.
    assert!(reject_teammate_cycle_target(&record, &chain, "Brand_Strategist").is_some());
    // An unresolvable key belongs to the grounding check, not to this one.
    assert_eq!(
        reject_teammate_cycle_target(&record, &chain, "nobody"),
        None
    );
}

/// The two chains share one vector and must not read each other's entries.
///
/// The direction that matters: a lead reached through a `delegate_to_desk`
/// hand-off has its own desk on the chain, and handing a slice to a peer on
/// that desk is precisely what #884 exists to allow. If the teammate guard
/// read desk entries it would refuse the feature's own headline case.
#[test]
fn the_desk_and_teammate_cycle_guards_do_not_read_each_others_entries() {
    let record = desk_record();
    let desk_chain = vec!["strategy".to_string()];
    assert_eq!(
        reject_teammate_cycle_target(&record, &desk_chain, "seo_specialist"),
        None,
        "a peer on the desk the chain came through is still reachable"
    );
    let agent_chain = vec![teammate_scope_key("brand_strategist")];
    assert_eq!(
        reject_cycle_target(&record, &agent_chain, "data", "chief"),
        None,
        "an agent entry is not a desk entry"
    );
}

/// Issue #884: the self-desk refusal is a dead end no longer — it names the
/// tool that can actually deliver the work, and who it can be delivered to.
#[test]
fn the_self_desk_refusal_names_the_teammate_tool() {
    let record = desk_record();
    let message =
        reject_cycle_target(&record, &[], "strategy", "brand_strategist").expect("rejected");
    assert!(message.contains("yourself"), "{message}");
    assert!(message.contains(DELEGATE_TO_TEAMMATE_TOOL), "{message}");
    assert!(
        message.contains("seo_specialist") && message.contains("copywriter"),
        "the peers it can reach are named: {message}"
    );

    // A lead with nobody else on its desk gets the old advice, not an empty
    // list and a tool that would refuse every target.
    let message = reject_cycle_target(&solo_record(), &[], "content", "writer").expect("rejected");
    assert!(!message.contains(DELEGATE_TO_TEAMMATE_TOOL), "{message}");
    assert!(message.contains("nobody else is on it"), "{message}");
}

#[test]
fn the_teammate_schema_takes_a_roster_id_and_an_instruction() {
    let schema = delegate_to_teammate_schema();
    assert_eq!(schema["required"], json!(["teammate", "instruction"]));
    assert_eq!(schema["additionalProperties"], json!(false));
}

/// The hosted manifest deliberately does NOT advertise the teammate tool —
/// that path has no per-agent routing to service it (issue #176).
#[test]
fn the_hosted_manifest_does_not_advertise_the_teammate_tool() {
    let names: Vec<String> = delegation_manifest_entries()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        !names.contains(&DELEGATE_TO_TEAMMATE_TOOL.to_string()),
        "{names:?}"
    );
}

/// An unresolvable key belongs to `reject_desk_target`, not to either of
/// the #176 checks — otherwise an invented desk would be reported as a
/// cycle or an allowlist miss and the model would go looking for the wrong
/// mistake.
#[test]
fn an_unknown_desk_is_left_to_the_grounding_check() {
    let record = record();
    assert_eq!(reject_cycle_target(&record, &[], "nowhere", "ceo"), None);
    assert_eq!(
        reject_out_of_allowlist_target(&record, &["content".into()], "nowhere"),
        None
    );
    // …and it IS refused by the check that owns it.
    assert!(reject_desk_target(&record, "nowhere").is_some());
}
