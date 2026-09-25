use super::types_test_support::*;
use super::*;

/// A stored override wins over the manifest in both directions — raising a
/// cap and lowering one. This is the "no redeploy" property at its source:
/// nothing here consults `company.toml` once a row exists.
#[test]
fn a_stored_override_beats_the_manifest() {
    let mut record = desk_record(BUDGET_ROSTER, Vec::new());
    record
        .overlay_budgets
        .push(budget_entry("analyst", Some(50.0)));
    assert_eq!(record.effective_budget("analyst"), Some(50.0));

    record.overlay_budgets = vec![budget_entry("analyst", Some(1.0))];
    assert_eq!(record.effective_budget("analyst"), Some(1.0));
}

/// The distinction the issue calls out by name: clearing a cap and setting
/// it to zero are different states and must not collapse into each other.
///
/// `Some(0.0)` caps the teammate at nothing (it will refuse to dispatch);
/// `None` means explicitly uncapped and beats the manifest's $5. If these
/// two ever resolved the same way, an operator lifting a cap would instead
/// have silenced the teammate completely — the opposite of what they asked
/// for, and unrecoverable from the console.
#[test]
fn clearing_a_cap_is_not_the_same_as_zeroing_it() {
    let mut record = desk_record(BUDGET_ROSTER, Vec::new());

    record.overlay_budgets = vec![budget_entry("analyst", Some(0.0))];
    assert_eq!(record.effective_budget("analyst"), Some(0.0));

    record.overlay_budgets = vec![budget_entry("analyst", None)];
    assert_eq!(
        record.effective_budget("analyst"),
        None,
        "an explicitly-uncapped override must beat the manifest's cap"
    );
}

/// An **overlay** teammate has no manifest row, so before #343 it could not
/// be capped at all. A stored override caps it like anyone else — and
/// dropping that override returns it to uncapped, since there is no manifest
/// value underneath to fall back to.
#[test]
fn an_overlay_teammate_can_be_capped() {
    let mut record = desk_record(BUDGET_ROSTER, Vec::new());
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "shane".to_string(),
        name: "Shane".to_string(),
        role: "Growth".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    assert_eq!(record.effective_budget("shane"), None);

    record.overlay_budgets = vec![budget_entry("shane", Some(2.5))];
    assert_eq!(record.effective_budget("shane"), Some(2.5));

    record.overlay_budgets.clear();
    assert_eq!(record.effective_budget("shane"), None);
}

/// Issue #343: one override per teammate. `upsert_budget_override` replaces
/// the held row instead of appending a second, so the cap an admin last set
/// is the cap every surface reads.
///
/// Appending would leave the *first* row winning `budget_override`'s
/// find-first read — meaning a raise or a revocation would persist happily
/// and change nothing, the failure mode hardest to notice from the console.
#[test]
fn upserting_an_override_replaces_rather_than_appends() {
    let mut record = desk_record(BUDGET_ROSTER, Vec::new());
    record.upsert_budget_override(budget_entry("analyst", Some(50.0)));
    record.upsert_budget_override(budget_entry("writer", Some(3.0)));
    record.upsert_budget_override(budget_entry("analyst", None));

    assert_eq!(
        record.overlay_budgets.len(),
        2,
        "a second write for one teammate must replace, not accumulate: {:?}",
        record.overlay_budgets
    );
    assert_eq!(
        record.effective_budget("analyst"),
        None,
        "the latest write must win over the manifest's $5"
    );
    assert_eq!(record.effective_budget("writer"), Some(3.0));
}

/// Issue #343: duplicates are detectable, so a caller holding overrides it
/// did not write (a bundle import) can refuse them instead of silently
/// applying whichever row happens to sort first.
#[test]
fn duplicate_overrides_are_detected() {
    let mut record = desk_record(BUDGET_ROSTER, Vec::new());
    assert_eq!(record.duplicate_budget_agent_id(), None);

    record.overlay_budgets = vec![
        budget_entry("analyst", Some(9.0)),
        budget_entry("writer", None),
    ];
    assert_eq!(
        record.duplicate_budget_agent_id(),
        None,
        "distinct teammates are not a duplicate"
    );

    // Two rows for one teammate that disagree about the cap — the case where
    // guessing would either over-restrict or hand back a revoked allowance.
    record.overlay_budgets = vec![
        budget_entry("analyst", Some(9.0)),
        budget_entry("writer", None),
        budget_entry("analyst", Some(0.0)),
    ];
    assert_eq!(record.duplicate_budget_agent_id(), Some("analyst"));
}

/// Issue #343: the budget overrides round-trip through the `OverlayBlob` the
/// sqlite/mongodb stores persist, and pre-#343 rows load as "no overrides"
/// (the manifest still decides) rather than failing to parse.
#[test]
fn overlay_blob_round_trips_budgets() {
    let mut record = desk_record(BUDGET_ROSTER, Vec::new());
    record.overlay_budgets = vec![
        budget_entry("analyst", Some(9.0)),
        budget_entry("writer", None),
    ];
    let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
    let blob = OverlayBlob::parse(&json).expect("reparse");
    assert_eq!(blob.budgets, record.overlay_budgets);

    let legacy = r#"{"agents":[],"desk_members":[]}"#;
    assert!(
        OverlayBlob::parse(legacy)
            .expect("pre-budget object")
            .budgets
            .is_empty()
    );
    assert!(
        OverlayBlob::parse("[]")
            .expect("legacy array")
            .budgets
            .is_empty()
    );
}

// ---- per-agent persona override (issue #1530) ------------------------

/// A roster with one manifest agent carrying a blueprint `prompt` and one
/// without — the two starting positions every persona-override case builds on.
const PERSONA_ROSTER: &str = "[company]\nname = \"Acme\"\n\
     [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\nprompt = \"Blueprint persona.\"\n\
     [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n";

fn override_entry(agent_id: &str, instructions: Option<&str>) -> AgentOverride {
    AgentOverride {
        agent_id: agent_id.to_string(),
        instructions: instructions.map(str::to_string),
        ..Default::default()
    }
}

/// A stored override wins over the manifest `prompt`: this is how a
/// manifest/blueprint agent's persona is edited without rewriting
/// `company.toml`.
#[test]
fn effective_instructions_prefers_override() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.overlay_agent_edits = vec![override_entry("ceo", Some("Be terse."))];
    assert_eq!(
        record.effective_instructions("ceo"),
        Some("Be terse.".to_string())
    );
}

/// With no override stored, the manifest `prompt` is returned verbatim — the
/// pre-#1530 behaviour, and the net that says adding the field changed
/// nothing for a company that never edits a persona.
#[test]
fn effective_instructions_falls_back_to_manifest_prompt() {
    let record = desk_record(PERSONA_ROSTER, Vec::new());
    assert_eq!(
        record.effective_instructions("ceo"),
        Some("Blueprint persona.".to_string())
    );
}

/// A bare overlay teammate (no manifest row) and a manifest agent that
/// declares no `prompt` both resolve to `None` when nothing overrides them.
#[test]
fn effective_instructions_none_for_bare_overlay_or_promptless_agent() {
    let record = desk_record(PERSONA_ROSTER, Vec::new());
    assert_eq!(record.effective_instructions("eng"), None);
    assert_eq!(record.effective_instructions("nobody"), None);
}

/// An override whose `instructions` is `None` carries nothing, so resolution
/// falls through to the blueprint — the "reset to blueprint" contract. A
/// stored empty-instructions row must never blank the persona.
#[test]
fn effective_instructions_empty_override_resets_to_blueprint() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.overlay_agent_edits = vec![override_entry("ceo", None)];
    assert_eq!(
        record.effective_instructions("ceo"),
        Some("Blueprint persona.".to_string()),
        "an override that carries no instructions must fall through to the manifest"
    );
}

/// `upsert_agent_override` replaces the teammate's row in place rather than
/// accumulating a second one — the invariant `agent_override`'s first-match
/// read depends on.
#[test]
fn upsert_agent_override_replaces_not_appends() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.upsert_agent_override(override_entry("ceo", Some("first")));
    record.upsert_agent_override(override_entry("ceo", Some("second")));
    assert_eq!(record.overlay_agent_edits.len(), 1);
    assert_eq!(
        record.effective_instructions("ceo"),
        Some("second".to_string())
    );
}

/// `clear_agent_override` drops the row so the blueprint applies again, and
/// is a no-op when nothing is stored.
#[test]
fn clear_agent_override_drops_the_row() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.upsert_agent_override(override_entry("ceo", Some("custom")));
    record.clear_agent_override("ceo");
    assert!(record.overlay_agent_edits.is_empty());
    assert_eq!(
        record.effective_instructions("ceo"),
        Some("Blueprint persona.".to_string())
    );
    // No-op when absent.
    record.clear_agent_override("ceo");
    assert!(record.overlay_agent_edits.is_empty());
}

// ---- per-agent avatar override --------------------------------------

/// Nobody has chosen until somebody does: an untouched roster resolves to
/// `None`, which the console renders as the mascot it hashes from the id.
#[test]
fn effective_avatar_is_none_until_chosen() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    assert_eq!(record.effective_avatar("ceo"), None);
    record.upsert_agent_override(AgentOverride {
        agent_id: "ceo".into(),
        avatar: Some("tiny:teal".into()),
        ..Default::default()
    });
    assert_eq!(record.effective_avatar("ceo"), Some("tiny:teal".into()));
}

/// An overlay teammate has no manifest row, and picks a face through the
/// same field — one override answers for both kinds of teammate.
#[test]
fn effective_avatar_answers_for_an_overlay_teammate() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "alex".into(),
        name: "Alex".into(),
        role: "Writer".into(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    record.upsert_agent_override(AgentOverride {
        agent_id: "alex".into(),
        avatar: Some("blob:01J8Z5Q9YQ".into()),
        ..Default::default()
    });
    assert_eq!(
        record.effective_avatar("alex"),
        Some("blob:01J8Z5Q9YQ".into())
    );
}

/// Resetting a face says nothing about the persona. The two clear paths
/// touch one field each, so neither can quietly undo the other's edit —
/// this is the regression the shared retain helper exists to prevent.
#[test]
fn clearing_one_override_field_leaves_the_others() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.upsert_agent_override(AgentOverride {
        agent_id: "ceo".into(),
        instructions: Some("Be terse.".into()),
        ..Default::default()
    });
    record.upsert_agent_override(AgentOverride {
        agent_id: "ceo".into(),
        avatar: Some("tiny:rose".into()),
        ..Default::default()
    });

    record.clear_agent_avatar("ceo");
    assert_eq!(record.effective_avatar("ceo"), None);
    assert_eq!(
        record.effective_instructions("ceo"),
        Some("Be terse.".to_string()),
        "resetting a face must not reset the persona"
    );

    record.clear_agent_override("ceo");
    assert!(
        record.overlay_agent_edits.is_empty(),
        "the row goes once it carries nothing"
    );
}

/// The mirror of the above, and the sharper half: an avatar-only override
/// must survive a persona reset. Before the shared retain helper, the
/// persona path's `retain` did not know the field existed and dropped the
/// whole row — resetting a persona silently reset the face too.
#[test]
fn clearing_the_persona_keeps_an_avatar_only_override() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.upsert_agent_override(AgentOverride {
        agent_id: "ceo".into(),
        avatar: Some("tiny:rose".into()),
        ..Default::default()
    });
    record.clear_agent_override("ceo");
    assert_eq!(record.effective_avatar("ceo"), Some("tiny:rose".into()));
}

/// Duplicates are detectable, so a caller holding overrides it did not write
/// (a bundle import) can refuse them rather than apply whichever sorts first.
#[test]
fn duplicate_override_agent_id_detects() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    assert_eq!(
        AgentOverride::duplicate_agent_id(&record.overlay_agent_edits),
        None
    );
    record.overlay_agent_edits = vec![
        override_entry("ceo", Some("a")),
        override_entry("eng", Some("b")),
        override_entry("ceo", Some("c")),
    ];
    assert_eq!(
        AgentOverride::duplicate_agent_id(&record.overlay_agent_edits),
        Some("ceo")
    );
}

/// An override carrying nothing is empty — and an override carrying only
/// `avatar`, `model` or `harness` is not, so a face-only edit or a
/// model-only edit is persisted rather than dropped as a no-op.
#[test]
fn agent_override_is_empty_only_when_nothing_is_set() {
    assert!(override_entry("ceo", None).is_empty());
    assert!(!override_entry("ceo", Some("x")).is_empty());

    for (field, fill) in [
        (
            "name",
            Box::new(|e: &mut AgentOverride| e.name = Some("Ada".to_string()))
                as Box<dyn Fn(&mut AgentOverride)>,
        ),
        (
            "role",
            Box::new(|e: &mut AgentOverride| e.role = Some("CEO".to_string())),
        ),
        (
            "description",
            Box::new(|e: &mut AgentOverride| e.description = Some("desc".to_string())),
        ),
        (
            "tools",
            Box::new(|e: &mut AgentOverride| e.tools = Some(Some(vec!["docs.*".to_string()]))),
        ),
        (
            "instructions",
            Box::new(|e: &mut AgentOverride| e.instructions = Some("Be terse.".to_string())),
        ),
        (
            "avatar",
            Box::new(|e: &mut AgentOverride| e.avatar = Some("tiny:teal".to_string())),
        ),
        (
            "model",
            Box::new(|e: &mut AgentOverride| e.model = Some("gpt-5".to_string())),
        ),
        (
            "harness",
            Box::new(|e: &mut AgentOverride| e.harness = Some("laptop".to_string())),
        ),
        (
            "provider",
            Box::new(|e: &mut AgentOverride| e.provider = Some("anthropic".to_string())),
        ),
    ] {
        let mut edit = override_entry("ceo", None);
        fill(&mut edit);
        assert!(
            !edit.is_empty(),
            "{field} alone must make the override non-empty"
        );
    }

    // The stored "cleared" form (`Some("")`, keys rework slice 3a) is also
    // non-empty — it is an edit an operator made, not a no-op, and
    // `retain_nonempty_agent_edits` must keep the row so the clear itself
    // is not silently forgotten.
    let mut cleared_provider = override_entry("ceo", None);
    cleared_provider.provider = Some(String::new());
    assert!(
        !cleared_provider.is_empty(),
        "a cleared provider must still count as an edit"
    );
}

/// `upsert_agent_override` carries the provider half of the pair (keys
/// rework slice 3a) exactly like `model`, and clearing it with `Some("")`
/// reads back as `None` on the effective agent while leaving an
/// untouched field (like `name`) alone.
#[test]
fn an_override_carries_and_clears_the_provider() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.upsert_agent_override(AgentOverride {
        agent_id: "ceo".into(),
        provider: Some("anthropic".to_string()),
        model: Some("test-model-large".to_string()),
        ..Default::default()
    });
    // Cloned rather than borrowed from `record`: `effective_manifest_agent`
    // is re-called after each further mutation below, and a borrow held
    // across those would conflict with `upsert_agent_override`'s `&mut self`.
    let manifest_agent = record
        .manifest
        .agents
        .iter()
        .find(|a| a.id == "ceo")
        .cloned()
        .expect("ceo is on the manifest");
    let effective = record.effective_manifest_agent(&manifest_agent);
    assert_eq!(effective.provider.as_deref(), Some("anthropic"));
    assert_eq!(effective.model.as_deref(), Some("test-model-large"));

    record.upsert_agent_override(AgentOverride {
        agent_id: "ceo".into(),
        provider: Some(String::new()),
        model: Some(String::new()),
        ..Default::default()
    });
    let effective = record.effective_manifest_agent(&manifest_agent);
    assert_eq!(effective.provider, None);
    assert_eq!(effective.model, None);

    // An upsert that names neither leaves the stored provider alone.
    record.upsert_agent_override(AgentOverride {
        agent_id: "ceo".into(),
        provider: Some("groq".to_string()),
        model: Some("test-model-small".to_string()),
        ..Default::default()
    });
    record.upsert_agent_override(AgentOverride {
        agent_id: "ceo".into(),
        name: Some("Robin".to_string()),
        ..Default::default()
    });
    let effective = record.effective_manifest_agent(&manifest_agent);
    assert_eq!(effective.provider.as_deref(), Some("groq"));
    assert_eq!(effective.name.as_deref(), Some("Robin"));
}

/// An `OverlayAgent`'s `provider` round-trips through JSON, and a record
/// written before the field existed deserializes to `None` and
/// re-serializes with no `provider` key — the same absent-means-unset
/// contract `harness` already has.
#[test]
fn an_overlay_agent_round_trips_its_provider() {
    let with_provider = OverlayAgent {
        provider: Some("anthropic".to_string()),
        id: "a".into(),
        name: "A".into(),
        role: "r".into(),
        description: None,
        tools: None,
        skills: None,
        model: Some("test-model-large".to_string()),
        harness: None,
    };
    let json = serde_json::to_value(&with_provider).unwrap();
    assert_eq!(json.get("provider"), Some(&serde_json::json!("anthropic")));
    let round: OverlayAgent = serde_json::from_value(json).unwrap();
    assert_eq!(round.provider.as_deref(), Some("anthropic"));

    let legacy: OverlayAgent =
        serde_json::from_str(r#"{"id":"a","name":"A","role":"r"}"#).expect("legacy overlay");
    assert_eq!(legacy.provider, None);
    let legacy_value = serde_json::to_value(&legacy).unwrap();
    assert!(
        legacy_value.get("provider").is_none(),
        "an absent provider must not serialize a `provider` key: {legacy_value}"
    );
}

/// The persona overrides round-trip through the `OverlayBlob` the
/// sqlite/mongodb stores persist, and pre-#1530 rows load as "no overrides"
/// (the manifest still decides) rather than failing to parse.
#[test]
fn overlay_blob_round_trips_agent_overrides() {
    let mut record = desk_record(PERSONA_ROSTER, Vec::new());
    record.overlay_agent_edits = vec![override_entry("ceo", Some("Be terse."))];
    let json = serde_json::to_string(&OverlayBlob::from_record(&record)).expect("serialize");
    let blob = OverlayBlob::parse(&json).expect("reparse");
    assert_eq!(blob.agent_edits, record.overlay_agent_edits);

    // A pre-#1530 object row (no `agent_overrides` key) loads as empty.
    let legacy = r#"{"agents":[],"desk_members":[]}"#;
    assert!(
        OverlayBlob::parse(legacy)
            .expect("pre-persona object")
            .agent_edits
            .is_empty()
    );
}

/// An operator-created overlay desk resolves through the same
/// `effective_desk_members` / `resolve_desk_id` / `desk_exists` helpers the
/// manifest desks use, so the REST list and the harness desk-lead resolver
/// treat it identically. Member additions still layer on top.
#[test]
fn overlay_desk_resolves_like_a_manifest_desk() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n";
    let mut record = desk_record(manifest, Vec::new());
    record.overlay_desks.push(OverlayDesk {
        id: "growth".into(),
        name: "Growth".into(),
        description: None,
        members: vec!["eng".into()],
        responder: crate::ports::types::ResponderMode::default(),
        hive: Default::default(),
    });
    // Resolves by id and by case-insensitive name.
    assert_eq!(record.resolve_desk_id("growth").as_deref(), Some("growth"));
    assert_eq!(record.resolve_desk_id("GROWTH").as_deref(), Some("growth"));
    assert!(record.desk_exists("growth"));
    // Founding member is the lead; a later overlay addition appends.
    assert_eq!(
        record.effective_desk_members("growth"),
        vec!["eng".to_string()]
    );
    record.overlay_desk_members.push(OverlayDeskMember {
        desk_id: "growth".into(),
        agent_id: "ceo".into(),
    });
    assert_eq!(
        record.effective_desk_members("growth"),
        vec!["eng".to_string(), "ceo".to_string()]
    );
}

/// An **overlay** desk never answers to a General spelling (issue #1743).
///
/// `POST .../desks` accepted `general`, `main` and the display name
/// `General` until that issue, so an upgraded record can be carrying one.
/// Every routing decision on the built-in `#general` channel funnels
/// through this one resolver — `desk_lead` → `responder_for` picks who
/// answers, and `mentioned_agents` picks who `@everyone` names — so a desk
/// that resolves here takes the company-wide line over: the console shows
/// `#general` while that desk's lead answers it, and a broadcast meant for
/// the whole roster reaches only that desk's members.
///
/// Keyed on the **key being asked for**, not on the desk, which is what
/// keeps this a narrowing of one question rather than a retirement: the
/// same desk still resolves under its own non-General id.
#[test]
fn an_overlay_desk_does_not_answer_to_a_general_spelling() {
    let manifest = "[company]\nname = \"Acme\"\n\
         [[agent]]\nid = \"ceo\"\nrole = \"Chief\"\n\
         [[agent]]\nid = \"eng\"\nrole = \"Engineer\"\n";
    let mut record = desk_record(manifest, Vec::new());
    record.overlay_desks.push(OverlayDesk {
        id: "main".into(),
        name: "Front office".into(),
        description: None,
        responder: Default::default(),
        members: vec!["eng".into()],
        hive: Default::default(),
    });
    record.overlay_desks.push(OverlayDesk {
        id: "ops".into(),
        name: "General".into(),
        description: None,
        responder: Default::default(),
        members: vec!["ceo".into()],
        hive: Default::default(),
    });

    for spelling in ["", "main", "Main", "MAIN", "general", "General"] {
        assert_eq!(
            record.resolve_desk_id(spelling),
            None,
            "an overlay desk must not answer to {spelling:?}"
        );
    }
    // Both desks still exist and still route under their own ids — this
    // narrows one question, it does not take a desk away.
    assert_eq!(record.resolve_desk_id("ops").as_deref(), Some("ops"));
    assert!(record.desk_exists("main"));
    assert_eq!(
        record.effective_desk_members("main"),
        vec!["eng".to_string()]
    );
}
