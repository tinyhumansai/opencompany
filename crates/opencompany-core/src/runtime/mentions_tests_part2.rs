use super::tests_core::*;

#[test]
fn an_agent_does_not_mention_itself_in_its_own_reply() {
    let sender = Actor {
        kind: ActorKind::Agent,
        id: "engineer".to_string(),
    };
    let found = resolve(
        "@engineer and @ceo",
        None,
        Some(&sender),
        &acme(),
        &people(),
    );
    assert_eq!(targets(&found), vec![&agent("ceo")]);
}

#[test]
fn a_repeated_mention_chips_twice_and_pings_once() {
    let found = resolve_text("@engineer ... @engineer again");
    assert_eq!(found.len(), 2, "both spans render");
    assert!(!found[0].quiet);
    assert!(found[1].quiet, "the second is render-only");
}

/// Past the cap the tail is demoted, never deleted — what a reader sees
/// still has to match what the author wrote.
#[test]
fn the_cap_demotes_the_tail_and_keeps_every_span() {
    let mentions: Vec<Mention> = (0..MENTION_CAP + 5)
        .map(|i| Mention {
            target: MentionTarget::User {
                id: format!("u{i}"),
            },
            text: format!("@u{i}"),
            offset: i * 8,
            quiet: false,
        })
        .collect();
    let out = normalize(mentions, None);
    assert_eq!(out.len(), MENTION_CAP + 5, "no span is dropped");
    assert_eq!(
        out.iter().filter(|m| !m.quiet).count(),
        MENTION_CAP,
        "exactly the cap pings"
    );
    assert!(out.last().expect("a tail mention").quiet);
}

#[test]
fn mentions_come_back_in_reading_order() {
    let found = resolve_text("@ceo then @engineer");
    let offsets: Vec<usize> = found.iter().map(|m| m.offset).collect();
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    assert_eq!(offsets, sorted);
}

// -----------------------------------------------------------------------
// revalidate: the client's answer is checked, not trusted
// -----------------------------------------------------------------------

#[test]
fn a_supplied_mention_for_a_missing_teammate_is_demoted_not_dropped() {
    let supplied = vec![Mention {
        target: agent("ghost"),
        text: "@ghost".to_string(),
        offset: 0,
        quiet: false,
    }];
    let out = resolve("@ghost hello", Some(supplied), None, &acme(), &people());
    assert_eq!(out.len(), 1, "the span survives so the text still matches");
    assert!(out[0].quiet, "but it pings nobody");
}

#[test]
fn a_supplied_span_that_is_not_in_the_text_is_dropped() {
    let supplied = vec![Mention {
        target: agent("engineer"),
        text: "@engineer".to_string(),
        offset: 40,
        quiet: false,
    }];
    let out = resolve("short message", Some(supplied), None, &acme(), &people());
    assert!(
        out.is_empty(),
        "a chip must never be drawn over text that says something else"
    );
}

/// A console that ran its picker and found nothing has given an answer; the
/// host must not then extract on its behalf.
#[test]
fn an_explicitly_empty_list_suppresses_extraction() {
    let out = resolve(
        "@engineer hello",
        Some(Vec::new()),
        None,
        &acme(),
        &people(),
    );
    assert!(out.is_empty());
}

#[test]
fn an_absent_list_falls_back_to_extraction() {
    let out = resolve("@engineer hello", None, None, &acme(), &people());
    assert_eq!(targets(&out), vec![&agent("engineer")]);
}

/// **Naming an outsider does not summon them into the room.**
///
/// `@designer` typed in `#engineering` used to make the designer answer
/// there — a desk they are not on. The mention now resolves to nobody, and
/// the ladder falls through to the desk's own answerer, who can carry the
/// question across on the referral path.
#[test]
fn a_teammate_from_another_desk_is_not_summoned_into_this_one() {
    let record = record(TWO_DESKS);
    let found = resolve(
        "@designer what would you change about the login screen",
        None,
        None,
        &record,
        &people(),
    );
    assert!(
        !found.is_empty(),
        "the mention must resolve, or this test passes for the wrong reason"
    );
    assert_eq!(
        mention_responder(&record, Some("engineering"), &found),
        None,
        "the designer answers on their own desk, not as a guest here"
    );
    assert_eq!(
        mention_responder(&record, Some("design"), &found),
        Some("designer".to_string()),
        "and is still the responder in the room they belong to"
    );
}

/// The rule needs a membership to check against. `#general` is not a desk
/// and has none (issue #1743), and neither has a DM — `resolve_desk_id`
/// says so by returning `None`, and there every roster agent stays
/// nameable, exactly as before.
#[test]
fn a_channel_with_no_membership_still_names_anybody() {
    let record = record(TWO_DESKS);
    let found = resolve(
        "@designer can you look at this",
        None,
        None,
        &record,
        &people(),
    );
    for channel in [None, Some("general"), Some("dm:u1:designer")] {
        assert_eq!(
            mention_responder(&record, channel, &found),
            Some("designer".to_string()),
            "{channel:?} has no membership to be outside of"
        );
    }
}

#[test]
fn a_mentioned_teammate_becomes_the_responder() {
    let found = resolve_text("@engineer what is the build status");
    assert_eq!(
        mention_responder(&acme(), None, &found),
        Some("engineer".to_string())
    );
}

#[test]
fn the_first_mentioned_teammate_answers() {
    let found = resolve_text("@ceo can you check with @engineer");
    assert_eq!(
        mention_responder(&acme(), None, &found),
        Some("ceo".to_string())
    );
}

#[test]
fn a_quiet_mention_never_routes() {
    let mentions = vec![Mention {
        target: agent("engineer"),
        text: "@engineer".to_string(),
        offset: 0,
        quiet: true,
    }];
    assert_eq!(mention_responder(&acme(), None, &mentions), None);
}

#[test]
fn an_off_roster_mention_falls_through_to_desk_routing() {
    let mentions = vec![Mention {
        target: agent("ghost"),
        text: "@ghost".to_string(),
        offset: 0,
        quiet: false,
    }];
    assert_eq!(
        mention_responder(&acme(), None, &mentions),
        None,
        "so the caller uses the desk lead, exactly as before"
    );
}

#[test]
fn mentioning_only_people_does_not_change_the_responder() {
    let found = resolve_text("@Jane Doe thoughts?");
    assert_eq!(mention_responder(&acme(), None, &found), None);
}

// -----------------------------------------------------------------------
// Expansion — a list for the turn's context, never a fan-out
// -----------------------------------------------------------------------

#[test]
fn everyone_expands_to_the_addressed_desks_members() {
    let found = resolve_text("@everyone standup in five");
    let named = mentioned_agents(&acme(), "engineering", &found, None);
    assert_eq!(named, vec!["engineer".to_string(), "ceo".to_string()]);
}

#[test]
fn everyone_notifies_every_person_in_the_company() {
    let found = resolve_text("@everyone standup in five");
    assert_eq!(
        mentioned_users(&people(), &found),
        vec!["u1".to_string(), "u2".to_string()],
        "a broadcast addresses the company's people, not a desk's teammates"
    );
}

/// `@everyone` on the built-in `#general` channel names the whole roster,
/// under every spelling the host folds into it (issue #1743).
///
/// Before this it named **nobody**: `#general` is not a desk, so
/// `resolve_desk_id` found nothing and the broadcast arm expanded against
/// an empty membership. The one channel where "everyone" literally means
/// everyone was the one channel where `@everyone` reached no one.
#[test]
fn everyone_on_the_general_channel_names_the_whole_roster() {
    let found = resolve_text("@everyone standup in five");
    for spelling in ["general", "General", "main", "Main", ""] {
        assert_eq!(
            mentioned_agents(&acme(), spelling, &found, None),
            vec!["engineer".to_string(), "ceo".to_string()],
            "@everyone addressed as {spelling:?} must name the whole roster"
        );
    }
}

/// An **overlay** desk that took a General spelling before those were
/// reserved must not narrow the company-wide broadcast (issue #1743).
///
/// `resolve_desk_id` matches a desk by id *or* by case-insensitive name, so
/// a persisted `{id: "ops", name: "General"}` is selected when
/// `HarnessBrain::everyone_desk` folds the built-in `main` thread to
/// `General` — and `@everyone` on the one channel where everyone means
/// everyone would reach only that desk's members. A desk the *blueprint*
/// declares is the company's own General desk and still wins; this is only
/// about state `create_desk` used to accept and now refuses.
#[test]
fn an_overlay_desk_squatting_a_general_spelling_does_not_narrow_the_broadcast() {
    let mut record = acme();
    record.overlay_desks.push(crate::ports::types::OverlayDesk {
        id: "ops".to_string(),
        name: "General".to_string(),
        description: None,
        responder: Default::default(),
        members: vec!["ceo".to_string()],
        hive: Default::default(),
    });
    let found = resolve_text("@everyone standup in five");
    for spelling in ["general", "General", "main", ""] {
        assert_eq!(
            mentioned_agents(&record, spelling, &found, None),
            vec!["engineer".to_string(), "ceo".to_string()],
            "@everyone addressed as {spelling:?} must still name the whole roster"
        );
    }
    // And the squatting desk keeps working as the desk it is, addressed by
    // its own id — this narrows the broadcast, nothing else.
    assert_eq!(
        mentioned_agents(&record, "ops", &found, None),
        vec!["ceo".to_string()],
        "the desk itself is unchanged"
    );
}

/// A named desk keeps expanding against **its own** membership, not the
/// roster — the reservation above must not leak into every channel.
#[test]
fn everyone_on_a_named_desk_still_names_only_that_desk() {
    let mut record = acme();
    // A teammate on nobody's desk: on the roster, off `#engineering`.
    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "designer".to_string(),
        name: "Dana".to_string(),
        role: "Designer".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });
    let found = resolve_text("@everyone standup in five");
    assert_eq!(
        mentioned_agents(&record, "engineering", &found, None),
        vec!["engineer".to_string(), "ceo".to_string()],
        "a desk broadcast is bounded by the desk"
    );
}

/// Membership of `#general` is **derived, never stored**: a teammate added
/// to the roster a moment ago is in it, with no membership write anywhere
/// (issue #1743).
///
/// The proof is the mutation, not the assertion: the only thing this test
/// changes is `overlay_agents` — the roster. `overlay_desk_members`,
/// `overlay_desks` and `overlay_desk_order` are asserted still empty, so
/// there is no second copy of "who is in #general" that could drift from
/// the roster. That is the whole reason the channel is not a desk.
#[test]
fn a_teammate_added_to_the_roster_is_in_general_with_no_membership_write() {
    let mut record = acme();
    let found = resolve_text("@everyone standup in five");
    assert_eq!(
        mentioned_agents(&record, "general", &found, None),
        vec!["engineer".to_string(), "ceo".to_string()]
    );

    record.overlay_agents.push(OverlayAgent {
        provider: None,
        id: "designer".to_string(),
        name: "Dana".to_string(),
        role: "Designer".to_string(),
        description: None,
        tools: None,
        skills: None,
        model: None,
        harness: None,
    });

    assert_eq!(
        mentioned_agents(&record, "general", &found, None),
        vec![
            "engineer".to_string(),
            "ceo".to_string(),
            "designer".to_string()
        ],
        "the new teammate is in #general the moment it joins the roster"
    );
    assert!(
        record.overlay_desk_members.is_empty()
            && record.overlay_desks.is_empty()
            && record.overlay_desk_order.is_empty(),
        "nothing was written to any desk overlay to make that true"
    );
}

/// A retired teammate drops out of `#general` on the same read, for the
/// same reason: `push` re-checks `is_roster_agent`, which is what a derived
/// membership buys — there is no stale seat to clean up.
#[test]
fn a_retired_teammate_leaves_general_on_the_next_read() {
    let mut record = acme();
    record.overlay_retired_agents.push("engineer".to_string());
    let found = resolve_text("@everyone standup in five");
    assert_eq!(
        mentioned_agents(&record, "general", &found, None),
        vec!["ceo".to_string()]
    );
}

#[test]
fn a_desk_mention_expands_to_that_desk_not_the_addressed_one() {
    let found = resolve_text("@engineering can you take this");
    let named = mentioned_agents(&acme(), "general", &found, None);
    assert_eq!(named, vec!["engineer".to_string(), "ceo".to_string()]);
}

#[test]
fn the_responder_is_not_told_it_was_mentioned() {
    let found = resolve_text("@engineer and @ceo");
    let named = mentioned_agents(&acme(), "engineering", &found, Some("engineer"));
    assert_eq!(named, vec!["ceo".to_string()]);
}

#[test]
fn expansion_deduplicates() {
    let found = resolve_text("@engineer and @engineering");
    let named = mentioned_agents(&acme(), "engineering", &found, None);
    assert_eq!(named, vec!["engineer".to_string(), "ceo".to_string()]);
}

#[test]
fn a_quiet_mention_expands_to_nothing() {
    let mentions = vec![Mention {
        target: MentionTarget::Everyone,
        text: "@everyone".to_string(),
        offset: 0,
        quiet: true,
    }];
    assert!(mentioned_agents(&acme(), "engineering", &mentions, None).is_empty());
    assert!(mentioned_users(&people(), &mentions).is_empty());
}

#[test]
fn a_person_who_has_left_is_not_notified() {
    let mentions = vec![Mention {
        target: MentionTarget::User {
            id: "gone".to_string(),
        },
        text: "@gone".to_string(),
        offset: 0,
        quiet: false,
    }];
    assert!(mentioned_users(&people(), &mentions).is_empty());
}

// -----------------------------------------------------------------------
// Labels
// -----------------------------------------------------------------------

#[test]
fn a_label_falls_back_from_display_name_to_a_derived_name() {
    assert_eq!(user_label(&user("u", "jane@x.test", Some("Jane"))), "Jane");
    // No chosen name: the same derived name `display_label` uses for the
    // profile pane, not the raw local part — the same person must read the
    // same way on a mention chip and in the people list.
    assert_eq!(user_label(&user("u", "jane.doe@x.test", None)), "Jane Doe");
    // A blanked display name is the same intent as `null`.
    assert_eq!(
        user_label(&user("u", "jane.doe@x.test", Some("  "))),
        "Jane Doe"
    );
    // An identity with no name in it to derive stays the honest fallback.
    assert_eq!(user_label(&user("u", "@x.test", None)), "someone");
}

#[test]
fn a_label_never_leaks_the_full_identity() {
    let label = user_label(&user("u", "jane@acme.test", None));
    assert!(!label.contains('@'), "{label}");
    assert!(!label.contains("acme.test"), "{label}");
}

// -----------------------------------------------------------------------
// Retired teammates
// -----------------------------------------------------------------------

#[test]
fn a_retired_teammate_is_not_mentionable() {
    let mut record = acme();
    record.overlay_retired_agents.push("engineer".to_string());
    let found = resolve("@engineer hello", None, None, &record, &people());
    assert!(found.is_empty(), "{found:?}");
}

/// Issue: a manifest teammate's operator-set display name (an
/// `AgentOverride`, applied through `effective_agents()`) must be a real
/// alias, not just its authored roster id — an operator who renamed
/// `ceo` to "Ada" expects `@Ada` to reach them.
#[test]
fn an_operator_renamed_manifest_agent_is_mentionable_by_the_new_name() {
    let mut record = acme();
    record.overlay_agent_edits.push(AgentOverride {
        agent_id: "ceo".to_string(),
        name: Some("Ada".to_string()),
        role: None,
        description: None,
        tools: None,
        instructions: None,
        avatar: None,
        ..Default::default()
    });
    let found = resolve("hey @Ada, got a sec?", None, None, &record, &people());
    assert_eq!(targets(&found), vec![&agent("ceo")], "{found:?}");
    // The authored id must keep working too — a rename is additive.
    let found = resolve("hey @ceo, got a sec?", None, None, &record, &people());
    assert_eq!(targets(&found), vec![&agent("ceo")], "{found:?}");
}

// -----------------------------------------------------------------------
// Client-supplied mentions must actually name their claimed target
// -----------------------------------------------------------------------

/// A caller cannot pair arbitrary text with a live target's id and have it
/// persisted as a real, notifying mention — the span must actually be a
/// spelling of that target. `@hello` is syntactically a mention (starts
/// with `@`, closes at the space), so this isolates the alias-mismatch
/// path from the syntax check covered separately below.
#[test]
fn a_supplied_mention_whose_text_does_not_name_its_target_is_demoted() {
    let supplied = vec![Mention {
        target: agent("engineer"),
        text: "@hello".to_string(),
        offset: 0,
        quiet: false,
    }];
    let out = resolve("@hello there", Some(supplied), None, &acme(), &people());
    assert_eq!(out.len(), 1, "the span survives so the text still matches");
    assert!(
        out[0].quiet,
        "text that never named the target must not ping it: {out:?}"
    );
}

/// Text that never had `@`-shape at all (no `@`, nowhere) is dropped
/// outright rather than kept and demoted — the same treatment a
/// mid-word or in-code-span match gets, and consistent with fallback
/// extraction, which would never have produced a mention here either.
#[test]
fn a_supplied_mention_with_no_at_sign_at_all_is_dropped() {
    let supplied = vec![Mention {
        target: agent("engineer"),
        text: "hello".to_string(),
        offset: 0,
        quiet: false,
    }];
    let out = resolve("hello there", Some(supplied), None, &acme(), &people());
    assert!(out.is_empty(), "{out:?}");
}

/// The picker is still trusted to disambiguate — a genuinely valid alias
/// for the claimed target stays a real, notifying mention.
#[test]
fn a_supplied_mention_whose_text_does_name_its_target_still_notifies() {
    let supplied = vec![Mention {
        target: agent("engineer"),
        text: "@engineer".to_string(),
        offset: 0,
        quiet: false,
    }];
    let out = resolve("@engineer hello", Some(supplied), None, &acme(), &people());
    assert_eq!(out.len(), 1);
    assert!(!out[0].quiet, "{out:?}");
}

/// A live alias sitting somewhere `opens_mention`/`closes_mention` would
/// refuse — mid-word, here — must be demoted the same way a mismatched
/// span is, not trusted just because the text happens to spell a real
/// alias.
#[test]
fn a_supplied_mention_mid_word_is_dropped() {
    let supplied = vec![Mention {
        target: agent("engineer"),
        text: "@engineer".to_string(),
        offset: 4,
        quiet: false,
    }];
    let out = resolve("jane@engineer", Some(supplied), None, &acme(), &people());
    assert!(
        out.is_empty(),
        "a span with no whitespace/bracket before it is not a mention: {out:?}"
    );
}

/// The same alias-shaped-but-not-a-mention rule applies inside a fenced or
/// inline code span — fallback extraction already masks these, and a
/// structured caller must not be able to route around that mask.
#[test]
fn a_supplied_mention_inside_a_code_span_is_dropped() {
    let text = "see `@engineer` for the review";
    let offset = text.find("@engineer").expect("span present");
    let supplied = vec![Mention {
        target: agent("engineer"),
        text: "@engineer".to_string(),
        offset,
        quiet: false,
    }];
    let out = resolve(text, Some(supplied), None, &acme(), &people());
    assert!(out.is_empty(), "{out:?}");
}

/// Two live targets can share an alias (two "Sam"s, say); a structured
/// caller submitting the identical span for both must not double-ping —
/// only the first-supplied target for that exact span survives.
#[test]
fn only_the_first_target_for_one_span_survives() {
    let users = vec![
        user("u1", "sam.one@acme.test", Some("Sam")),
        user("u2", "sam.two@acme.test", Some("Sam")),
    ];
    let supplied = vec![
        Mention {
            target: MentionTarget::User {
                id: "u1".to_string(),
            },
            text: "@Sam".to_string(),
            offset: 0,
            quiet: false,
        },
        Mention {
            target: MentionTarget::User {
                id: "u2".to_string(),
            },
            text: "@Sam".to_string(),
            offset: 0,
            quiet: false,
        },
    ];
    let out = resolve("@Sam please review", Some(supplied), None, &acme(), &users);
    assert_eq!(
        out.len(),
        1,
        "one run of text cannot name two different people: {out:?}"
    );
    assert_eq!(
        out[0].target,
        MentionTarget::User {
            id: "u1".to_string()
        },
        "the picker's own ordering decides which of the pair is honoured: {out:?}"
    );
}

// -----------------------------------------------------------------------
// Unicode
// -----------------------------------------------------------------------

/// A display name that starts with a non-ASCII letter is still a real
/// alias `directory` offers verbatim — extraction must open a mention on
/// it exactly as it does on an ASCII one.
#[test]
fn a_non_ascii_display_name_opens_a_mention() {
    let users = vec![user("u1", "elodie@acme.test", Some("Élodie"))];
    let found = resolve("hey @Élodie, can you look?", None, None, &acme(), &users);
    assert_eq!(
        targets(&found),
        vec![&MentionTarget::User {
            id: "u1".to_string()
        }],
        "{found:?}"
    );
}

/// A multi-byte character landing where a short alias's span would end
/// must not panic — it simply does not match, the same as any other
/// non-matching text.
#[test]
fn a_multibyte_character_at_a_short_aliass_boundary_does_not_panic() {
    let users = vec![user("u1", "j@acme.test", Some("J"))];
    // "é" is two UTF-8 bytes; a one-character alias ("j") ends inside it.
    let found = resolve("@é hello", None, None, &acme(), &users);
    assert!(found.is_empty(), "{found:?}");
}
