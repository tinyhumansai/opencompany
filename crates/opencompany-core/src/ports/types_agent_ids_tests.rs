use super::types_test_support::*;
use super::*;

/// Desks resolve *before* teammates in `assignee::resolve`, by id and by
/// case-insensitive display name — so a minted id equal to either would be
/// unreachable, and both are stepped past.
#[test]
fn mint_agent_id_steps_past_desk_ids_and_desk_names() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[group_chat]]\nid = \"growth\"\nname = \"Content\"\nmembers = [\"ceo\"]\n";
    let mut record = desk_record(manifest, Vec::new());

    // By desk id.
    assert_eq!(record.mint_agent_id("Growth"), "growth_2");
    // By desk display name, which `resolve_desk_id` matches ignoring case.
    assert_eq!(record.mint_agent_id("content"), "content_2");

    // A desk name that is not itself slug-shaped is *not* reserved: nothing
    // routes on the slug of a desk name, only on the name as written, so
    // `content_desk` shadows no key that "Content Desk" answers to.
    record.overlay_desks.push(OverlayDesk {
        id: "design".into(),
        name: "Design Studio".into(),
        description: None,
        members: Vec::new(),
        responder: crate::ports::types::ResponderMode::default(),
        hive: Default::default(),
    });
    assert_eq!(record.mint_agent_id("Design Studio"), "design_studio");
    // …while the overlay desk's id is reserved exactly like a manifest one.
    assert_eq!(record.mint_agent_id("Design"), "design_2");
}

/// The operator channel and the workspace system roots are never handed to
/// a teammate, on an otherwise empty roster.
#[test]
fn mint_agent_id_never_returns_a_reserved_id() {
    let record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
    assert_eq!(record.mint_agent_id("Operator"), "operator_2");
    assert_eq!(record.mint_agent_id("Agents"), "agents_2");
    assert_eq!(record.mint_agent_id("desks"), "desks_2");
    assert_eq!(record.mint_agent_id("System"), "system_2");
    // Issue #1743: both spellings of the built-in `#general` channel. A
    // teammate minted onto one becomes the answer to every unaddressed
    // message on the company-wide line — `responder_for` checks roster ids
    // before falling back to the orchestrator — and the console renders
    // that line's transcript as the teammate's DM.
    assert_eq!(record.mint_agent_id("Main"), "main_2");
    assert_eq!(record.mint_agent_id("General"), "general_2");
    assert_eq!(
        RESERVED_AGENT_IDS,
        ["operator", "agents", "desks", "system", "main", "General"]
    );
}

/// Issue #966: the host's own author is not a name a teammate can be given.
///
/// `SYSTEM_AUTHOR` reaches the console's centred system pill by value —
/// `MessageView` projects an `AgentReply`'s `agent_id` straight into
/// `author`, and the console keys on the string. A teammate holding that id
/// would therefore render *as the host*, which is a worse confusion than the
/// one this issue set out to fix, and the value it replaces (`"operator"`)
/// was already reserved.
///
/// Its sibling `CONFINED_AGENT_ID` needs no entry here: `agent_slug` emits
/// only lowercase alphanumerics and underscores, so `"workflow-copilot"` is
/// unmintable by construction. `"system"` is an ordinary legal slug.
#[test]
fn mint_agent_id_never_returns_the_host_author() {
    let record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
    assert_eq!(
        agent_slug("System"),
        crate::ports::SYSTEM_AUTHOR,
        "the guard is needed precisely because this is a legal slug"
    );
    assert_ne!(
        record.mint_agent_id("System"),
        crate::ports::SYSTEM_AUTHOR,
        "a teammate must never be minted onto the id the runtime speaks under"
    );
}

/// Issue #1162: the resolve every surface that takes a teammate key runs.
/// An id resolves, an overlay teammate's **display name** resolves to the
/// id it was minted under, and a key that is nobody resolves to nothing.
#[test]
fn resolve_teammate_key_takes_an_id_or_a_display_name() {
    let manifest = "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
    let mut record = desk_record(manifest, Vec::new());
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "dana_designer".into(),
        name: "Dana Designer".into(),
        role: "Designer".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });

    assert_eq!(
        record.resolve_teammate_key("ceo"),
        TeammateResolution::Agent("ceo".into())
    );
    assert_eq!(
        record.resolve_teammate_key("dana_designer"),
        TeammateResolution::Agent("dana_designer".into())
    );
    // The case #1162 is about: the name `query_company` prints, grounding
    // to the id the delegation tools accept.
    assert_eq!(
        record.resolve_teammate_key("Dana Designer"),
        TeammateResolution::Agent("dana_designer".into())
    );
    assert_eq!(
        record.resolve_teammate_key("  dana designer  "),
        TeammateResolution::Agent("dana_designer".into())
    );
    assert_eq!(
        record.resolve_teammate_key("ghost"),
        TeammateResolution::Unknown
    );
    assert_eq!(
        record.resolve_teammate_key("   "),
        TeammateResolution::Unknown
    );
}

/// Ids win. A teammate whose **display name** is another teammate's id can
/// never intercept work meant for that id — the ordering is the guarantee
/// that makes one shared resolver safe to use everywhere.
#[test]
fn resolve_teammate_key_never_lets_a_name_shadow_an_id() {
    let manifest = "[company]\nname = \"Acme\"\n[[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n";
    let mut record = desk_record(manifest, Vec::new());
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "impostor".into(),
        name: "ceo".into(),
        role: "Growth".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    assert_eq!(
        record.resolve_teammate_key("ceo"),
        TeammateResolution::Agent("ceo".into())
    );
}

/// Two teammates answering to one display name is a collision the operator
/// created, and it is reported as one: every colliding id comes back, so a
/// caller can name them instead of silently taking the first.
#[test]
fn resolve_teammate_key_reports_a_name_two_teammates_answer_to() {
    let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
    for id in ["dana_designer", "dana_designer_2"] {
        record.overlay_agents.push(OverlayAgent {
            provider: None,
            id: id.into(),
            name: "Dana Designer".into(),
            role: "Designer".into(),
            description: None,
            tools: None,
            skills: None,
            model: None,
            harness: None,
        });
    }
    assert_eq!(
        record.resolve_teammate_key("dana designer"),
        TeammateResolution::Ambiguous(vec!["dana_designer".into(), "dana_designer_2".into()])
    );
    // Either id still resolves on its own — the collision is in the name.
    assert_eq!(
        record.resolve_teammate_key("dana_designer_2"),
        TeammateResolution::Agent("dana_designer_2".into())
    );
}

/// Whatever is minted is a legal roster id, suffix included — the same
/// grammar the manifest validator holds a hand-authored id to.
#[test]
fn every_minted_id_satisfies_the_manifest_id_grammar() {
    let mut record = mint_record();
    for name in [
        "Dana Designer",
        "Backend Engineer",
        "***",
        "24/7 Support",
        "Operator",
        "設計者",
    ] {
        let id = record.mint_agent_id(name);
        assert!(
            crate::company::is_snake_case(&id),
            "minted id {id:?} from {name:?} is not a legal roster id"
        );
        add_overlay(&mut record, &id, name);
    }
}

/// The persisted overlay blob reads both the current object form and the
/// legacy bare-`overlay_agents`-array form, so existing sqlite/mongo rows
/// load without a migration.
#[test]
fn overlay_blob_parses_object_and_legacy_array() {
    let object = r#"{"agents":[{"id":"a","name":"A","role":"r"}],"desk_members":[{"desk_id":"d","agent_id":"a"}]}"#;
    let blob = OverlayBlob::parse(object).expect("object");
    assert_eq!(blob.agents.len(), 1);
    assert_eq!(blob.desk_members.len(), 1);
    // Issue #85: an object written before provenance existed omits the key;
    // `#[serde(default)]` loads it as `None` (zero-migration back-compat).
    assert!(blob.provenance.is_none());

    // Legacy: overlay_json used to hold a bare Vec<OverlayAgent>.
    let legacy = r#"[{"id":"a","name":"A","role":"r"}]"#;
    let blob = OverlayBlob::parse(legacy).expect("legacy array");
    assert_eq!(blob.agents.len(), 1);
    assert!(blob.desk_members.is_empty());
    assert!(blob.provenance.is_none());

    // The empty-array default persisted by fresh schema.
    let blob = OverlayBlob::parse("[]").expect("empty array");
    assert!(blob.agents.is_empty());
    assert!(blob.desk_members.is_empty());
    assert!(blob.provenance.is_none());
    assert!(blob.desks.is_empty());

    // A pre-desk-creation object row (no `desks` key) loads with an empty
    // desk overlay — no migration needed.
    let pre_desks = r#"{"agents":[],"desk_members":[]}"#;
    let blob = OverlayBlob::parse(pre_desks).expect("pre-desks object");
    assert!(blob.desks.is_empty());
}

/// Issue #85: a record's template provenance round-trips through the
/// `OverlayBlob` the sqlite/mongodb stores persist, and a blob carrying
/// provenance re-parses with it intact.
#[test]
fn overlay_blob_carries_template_provenance() {
    let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
    record.template_provenance = Some(TemplateProvenance {
        source_id: "law_firm".to_string(),
        version: Some("2.0.0".to_string()),
        path: Some("companies/law_firm".to_string()),
    });
    let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
    let blob = OverlayBlob::parse(&json).expect("reparse");
    assert_eq!(blob.provenance, record.template_provenance);
}

/// Issue #168: a runtime-authored workflow body round-trips through the
/// `OverlayBlob` the sqlite/mongodb stores persist as `overlay_json`. On a
/// hosted tenant this blob is the ONLY copy of the graph, so a serialization
/// gap here would silently delete the workflow.
#[test]
fn overlay_blob_round_trips_workflows() {
    let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
    record.overlay_workflows.push(OverlayWorkflow {
        id: "greeter".to_string(),
        toml: "id = \"greeter\"\nname = \"Greeter\"\n".to_string(),
    });
    let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
    let blob = OverlayBlob::parse(&json).expect("reparse");
    assert_eq!(blob.workflows, record.overlay_workflows);

    // A row written before workflow bodies persisted (no `workflows` key)
    // loads as empty — no migration needed.
    let legacy = r#"{"agents":[],"desk_members":[]}"#;
    assert!(
        OverlayBlob::parse(legacy)
            .expect("pre-workflows object")
            .workflows
            .is_empty()
    );
    // …and so does the legacy bare-array form.
    assert!(
        OverlayBlob::parse("[]")
            .expect("legacy array")
            .workflows
            .is_empty()
    );
}

/// Issue #276: the paused-workflow ids ride the same overlay blob as the
/// graph bodies, reconstructed on load by both string-column stores
/// (`sqlite` and `mongodb` read `OverlayBlob::parse`). A round trip here
/// pins that the field is not dropped in `from_record`/`parse`.
#[test]
fn overlay_blob_round_trips_disabled_workflows() {
    let mut record = desk_record("[company]\nname = \"Acme\"\n", Vec::new());
    record.disabled_workflows.push("digest".to_string());
    let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
    let blob = OverlayBlob::parse(&json).expect("reparse");
    assert_eq!(blob.disabled_workflows, record.disabled_workflows);

    // A row written before the pause switch existed holds no `disabled_workflows`
    // key and loads as empty — the pre-#276 behaviour, no migration needed.
    let legacy = r#"{"agents":[],"desk_members":[]}"#;
    assert!(
        OverlayBlob::parse(legacy)
            .expect("pre-#276 object")
            .disabled_workflows
            .is_empty()
    );
    assert!(
        OverlayBlob::parse("[]")
            .expect("legacy array")
            .disabled_workflows
            .is_empty()
    );
}

/// A manifest with two teammates, one capped at $5/day and one uncapped —
/// the two starting positions every budget-override case builds on.
const BUDGET_ROSTER: &str = "[company]\nname = \"Acme\"\n\
     [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\nbudget_usd_daily = 5.0\n\
     [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n";

// ---- `[policy]` override (issue #562) --------------------------------

const POLICY_MANIFEST: &str = "[company]\nname = \"Acme\"\n\
     [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\n\
     [policy]\nmode = \"supervised\"\n\
     always_approve = [\"payment.send\", \"filing.submit\"]\n";

fn policy_entry(mode: Option<&str>, always: Option<Vec<&str>>) -> PolicyOverride {
    PolicyOverride {
        mode: mode.map(str::to_string),
        always_approve: always.map(|v| v.into_iter().map(str::to_string).collect()),
        auto_approve_under_usd: None,
        approval_ttl_hours: None,
        set_by: Actor {
            kind: ActorKind::User,
            id: "user-1".to_string(),
        },
        at_millis: 1_700_000_000_000,
    }
}

#[test]
fn explicit_no_cap_policy_override_survives_json_round_trip() {
    let mut override_ = policy_entry(None, None);
    override_.auto_approve_under_usd = Some(None);
    let encoded = serde_json::to_value(&override_).expect("serialize override");
    assert!(encoded["auto_approve_under_usd"].is_null());
    let decoded: PolicyOverride = serde_json::from_value(encoded).expect("deserialize override");
    assert_eq!(decoded.auto_approve_under_usd, Some(None));
}

/// With no override stored, `effective_policy` is the manifest verbatim —
/// the pre-#562 behaviour, and the net that says adding this field changed
/// nothing for a company that never uses it.
#[test]
fn effective_policy_falls_back_to_the_manifest() {
    let record = desk_record(POLICY_MANIFEST, Vec::new());
    let effective = record.effective_policy();
    assert_eq!(effective.mode, "supervised");
    assert_eq!(
        effective.always_approve,
        vec!["payment.send", "filing.submit"]
    );
}

/// A stored override beats the manifest. This is the "no redeploy" property
/// at its source: nothing here consults `company.toml` once a row exists.
#[test]
fn a_stored_policy_override_beats_the_manifest() {
    let mut record = desk_record(POLICY_MANIFEST, Vec::new());
    record.overlay_policy = Some(policy_entry(Some("full"), None));
    assert_eq!(record.effective_policy().mode, "full");
}

/// Version skew can leave an override written by a newer host on a build
/// that does not recognise its tier. Falling through to `supervised` would
/// loosen a `readonly` seed, so the manifest wins for that field while any
/// independently valid always-ask override remains in force.
#[test]
fn an_unknown_stored_policy_mode_cannot_loosen_the_manifest() {
    let manifest = POLICY_MANIFEST.replace("mode = \"supervised\"", "mode = \"readonly\"");
    let mut record = desk_record(&manifest, Vec::new());
    record.overlay_policy = Some(policy_entry(
        Some("future-tier"),
        Some(vec!["external.publish"]),
    ));

    let effective = record.effective_policy();
    assert_eq!(effective.mode, "readonly");
    assert_eq!(effective.always_approve, vec!["external.publish"]);
}

/// The two fields are independent: moving the tier must not silently reset
/// the always-ask list to the manifest's, nor the reverse.
///
/// This is the merge that makes the console usable — the tier control and
/// the always-ask editor are separate widgets, and each `PUT` names only
/// what it changed. If either field reset the other, using one control would
/// quietly undo the other, and the always-ask list is the operator's real
/// lever: it wins over every tier including `full`.
#[test]
fn overriding_one_policy_field_leaves_the_other_alone() {
    let mut record = desk_record(POLICY_MANIFEST, Vec::new());

    record.overlay_policy = Some(policy_entry(Some("full"), None));
    let effective = record.effective_policy();
    assert_eq!(effective.mode, "full");
    assert_eq!(
        effective.always_approve,
        vec!["payment.send", "filing.submit"],
        "moving the tier must not discard the manifest's always-ask list"
    );

    record.overlay_policy = Some(policy_entry(None, Some(vec!["external.publish"])));
    let effective = record.effective_policy();
    assert_eq!(
        effective.mode, "supervised",
        "editing the always-ask list must not move the tier"
    );
    assert_eq!(effective.always_approve, vec!["external.publish"]);
}

/// An emptied always-ask list is a real state, not a fallback.
///
/// `Some(vec![])` is an operator deliberately clearing the list; `None` is
/// "not overridden". If these collapsed, an operator clearing the list would
/// instead get the manifest's three defaults back — silently re-imposing the
/// gates they had just removed, and with no way to express what they meant.
#[test]
fn an_emptied_always_approve_list_is_not_a_fallback() {
    let mut record = desk_record(POLICY_MANIFEST, Vec::new());

    record.overlay_policy = Some(policy_entry(None, Some(vec![])));
    assert!(
        record.effective_policy().always_approve.is_empty(),
        "an explicitly emptied always-ask list must survive as empty"
    );

    record.overlay_policy = Some(policy_entry(None, None));
    assert_eq!(
        record.effective_policy().always_approve,
        vec!["payment.send", "filing.submit"],
        "an absent field must fall through to the manifest"
    );
}

/// The spend threshold and deadline are overridden independently of the
/// tier and list, including an explicit no-cap choice.
#[test]
fn spend_threshold_and_deadline_can_be_overridden_independently() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\n\
         [policy]\nmode = \"supervised\"\nauto_approve_under_usd = 2.5\n";
    let mut record = desk_record(manifest, Vec::new());
    record.overlay_policy = Some(policy_entry(Some("full"), Some(vec![])));
    assert_eq!(record.effective_policy().auto_approve_under_usd, Some(2.5));
    assert_eq!(record.effective_policy().approval_ttl_hours, None);

    let override_ = record.overlay_policy.as_mut().unwrap();
    override_.auto_approve_under_usd = Some(None);
    override_.approval_ttl_hours = Some(72);
    let effective = record.effective_policy();
    assert_eq!(effective.auto_approve_under_usd, None);
    assert_eq!(effective.approval_ttl_hours, Some(72));
}

/// The roster a company was launched with is still the roster it runs, until
/// somebody edits it: with no override stored, every field reads straight off
/// the manifest. The regression net that says adding this layer changed
/// nothing for a company that never uses it.
#[test]
fn an_unedited_teammate_reads_straight_off_the_manifest() {
    let record = desk_record(EDIT_ROSTER, Vec::new());
    let analyst = record.effective_agent("analyst").expect("on the roster");
    assert!(matches!(analyst, std::borrow::Cow::Borrowed(_)));
    assert_eq!(analyst.role, "Analyst");
    assert_eq!(analyst.description.as_deref(), Some("Weighs evidence."));
    assert_eq!(analyst.name, None);
    assert!(record.effective_agent("nobody").is_none());
}

/// An edit wins over the blueprint, field by field — and only field by
/// field: what nobody touched keeps tracking `company.toml`, so a redeploy
/// that changes it is still felt.
#[test]
fn an_edit_wins_per_field_and_the_rest_still_tracks_the_manifest() {
    let mut record = desk_record(EDIT_ROSTER, Vec::new());
    record.upsert_agent_override(AgentOverride {
        agent_id: "analyst".to_string(),
        role: Some("Chief Vibes".to_string()),
        name: Some("Robin".to_string()),
        ..Default::default()
    });

    let analyst = record.effective_agent("analyst").expect("on the roster");
    assert_eq!(analyst.role, "Chief Vibes");
    assert_eq!(analyst.name.as_deref(), Some("Robin"));
    assert_eq!(
        analyst.description.as_deref(),
        Some("Weighs evidence."),
        "an untouched field must still come from the manifest"
    );
    assert_eq!(
        analyst.tools,
        Some(vec!["workspace.read".to_string()]),
        "and so must an untouched tool line"
    );
    // The blueprint itself is never rewritten — that is the whole point of
    // storing this as an overlay.
    assert_eq!(record.manifest.agents[0].role, "Analyst");
}

/// A stored empty description is the operator clearing it, not a teammate
/// whose instructions are the empty string. Collapsing the two would leave a
/// cleared description silently re-inheriting the blueprint's.
#[test]
fn a_cleared_description_stays_cleared() {
    let mut record = desk_record(EDIT_ROSTER, Vec::new());
    record.upsert_agent_override(AgentOverride {
        agent_id: "analyst".to_string(),
        description: Some(String::new()),
        ..Default::default()
    });
    assert_eq!(
        record.effective_agent("analyst").unwrap().description,
        None,
        "a cleared description must not fall back to the manifest's"
    );
}

/// Two patches of different fields are one override, merged — never two
/// rows, of which `effective_agent` would read whichever came first.
#[test]
fn a_second_edit_merges_rather_than_duplicating() {
    let mut record = desk_record(EDIT_ROSTER, Vec::new());
    record.upsert_agent_override(AgentOverride {
        agent_id: "analyst".to_string(),
        role: Some("Chief Vibes".to_string()),
        ..Default::default()
    });
    record.upsert_agent_override(AgentOverride {
        agent_id: "analyst".to_string(),
        // Double-option since #1804: `Some(Some(globs))` narrows.
        tools: Some(Some(vec!["composio".to_string()])),
        ..Default::default()
    });

    assert_eq!(record.overlay_agent_edits.len(), 1);
    let analyst = record.effective_agent("analyst").unwrap();
    assert_eq!(analyst.role, "Chief Vibes", "the earlier edit survives");
    assert_eq!(analyst.tools, Some(vec!["composio".to_string()]));
}

/// A removed teammate is off the roster everywhere the roster is read: the
/// effective list, the per-id lookup, and the membership predicate the desk
/// overlay validates against. Anything that still answered `true` here would
/// be a surface on which a deleted teammate is still addressable.
#[test]
fn a_retired_teammate_is_off_the_roster() {
    let mut record = desk_record(EDIT_ROSTER, Vec::new());
    assert!(record.is_roster_agent("analyst"));

    record.retire_agent("analyst");
    assert!(record.is_retired("analyst"));
    assert!(record.effective_agent("analyst").is_none());
    assert!(record.effective_agents().is_empty());
    assert!(!record.is_roster_agent("analyst"));
    // The blueprint is untouched — the tombstone is what removes it, which
    // is the only thing that survives the manifest being re-read on load.
    assert_eq!(record.manifest.agents[0].id, "analyst");
}

/// Retiring twice is one tombstone. A second entry changes nothing about the
/// roster but does move the harness's overlay fingerprint, which would drop
/// every live agent session for a delete that had already happened.
#[test]
fn retiring_a_teammate_twice_records_one_tombstone() {
    let mut record = desk_record(EDIT_ROSTER, Vec::new());
    record.retire_agent("analyst");
    record.retire_agent("analyst");
    assert_eq!(record.overlay_retired_agents, vec!["analyst".to_string()]);
}

/// A removed teammate loses its blueprint desk seat too. Left in place it
/// would still lead the desk, still take `delegate_to_desk` hand-offs and
/// still sit on the org chart — a delete that removed the card and nothing
/// else.
#[test]
fn a_retired_teammate_loses_its_desk_seat() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\n\
         [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n\
         [[group_chat]]\nid = \"studio\"\nname = \"Studio\"\n\
         members = [\"analyst\", \"writer\"]\n";
    let mut record = desk_record(manifest, Vec::new());
    assert_eq!(
        record.effective_desk_members("studio"),
        ["analyst", "writer"]
    );

    record.retire_agent("analyst");
    assert_eq!(
        record.effective_desk_members("studio"),
        ["writer"],
        "and the desk's lead moves to whoever is actually left"
    );
}

const EDIT_ROSTER: &str = "[company]\nname = \"Acme\"\n\
     [[agent]]\nid = \"analyst\"\nrole = \"Analyst\"\n\
     description = \"Weighs evidence.\"\ntools = [\"workspace.read\"]\n";

/// Issue #343: with no override stored, `effective_budget` is the manifest
/// value verbatim — the pre-#343 behaviour, and the regression net that says
/// adding this field changed nothing for a company that never uses it.
#[test]
fn effective_budget_falls_back_to_the_manifest() {
    let record = desk_record(BUDGET_ROSTER, Vec::new());
    assert_eq!(record.effective_budget("analyst"), Some(5.0));
    assert_eq!(record.effective_budget("writer"), None);
    // An id on no roster at all is uncapped rather than an error: the gate
    // reads this per dispatched agent and must not invent a cap.
    assert_eq!(record.effective_budget("nobody"), None);
}
