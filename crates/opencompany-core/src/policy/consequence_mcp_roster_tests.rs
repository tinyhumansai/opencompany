use super::consequence_hosting_tests::*;
use super::*;
use serde_json::json;

/// Issue #443: the agent persona instructs every agent to call these rather
/// than answer a capability question from memory. They read local
/// registration state and reach nothing.
#[test]
pub(super) fn listing_mcp_tools_never_parks_but_calling_through_a_server_does() {
    for tool in ["mcp_list_tools", "mcp_registry_list_tools"] {
        assert_eq!(c(tool).reach, Reach::Nothing, "`{tool}` reads local state");
    }
    for tool in ["mcp_call_tool", "mcp_registry_tool_call"] {
        assert!(
            c(tool).reach.parks_under_supervision(),
            "`{tool}` can perform any effect the remote server advertises"
        );
    }
}

/// `mcp_list_servers` answers with each configured server's credentials, so it
/// is kept out of every company agent's scope rather than classified as the read
/// its name suggests. Undeclared is therefore the intended state, and an
/// undeclared name grades as a per-call consequence — the safe direction.
///
/// Asserted here so that putting the name back in the roster as a free read has
/// to be a deliberate change to this test, not a quiet one.
#[test]
pub(super) fn listing_the_configured_servers_is_not_a_free_read_for_a_company_agent() {
    let listing = c("mcp_list_servers");
    assert!(
        listing.reach.parks_under_supervision(),
        "an undeclared `mcp_list_servers` must not grade as a read: {listing:?}"
    );
    assert_eq!(listing.standing, Standing::PerCall);
}

/// The sibling defects the same sweep turned up: four pure reads of the
/// agent's own workspace that parked because the read-only-prefix rule
/// keys on the *start* of a name and none of them begins with one.
///
/// `read_workspace_state` was in this list until issue #459 showed it is
/// not a read at all — see
/// [`reading_workspace_state_is_classified_with_shell_because_it_runs_git`].
#[test]
pub(super) fn a_workspace_read_never_parks_whatever_its_name_begins_with() {
    for tool in [
        "file_read",
        "glob",
        "grep",
        "image_info",
        "list",
        "memory_recall",
        "workspace_list",
        "workspace_read",
        "workspace_search",
        "media_list_models",
        "composio_list_toolkits",
        "composio_list_connections",
        "composio_list_tools",
    ] {
        assert_eq!(c(tool).reach, Reach::Nothing, "`{tool}` is a read");
    }
}

/// Issue #459: `read_workspace_state` is not the read its name promises.
/// It runs `git` in `{root}/{company}/{agent}/workspace`, and git reads
/// `.git/config` from that directory — a file the agent's own `file_write`
/// can author, and one whose keys can name a command to run. Until the
/// vendored `run_git` refuses untrusted repository config, it is
/// classified with `shell`.
///
/// The standing assertion is the one that matters most: `file_write` is
/// grantable, so if this were grantable too, the pair could be handed over
/// together for a week and the hole would be open for the length of the
/// grant with nobody watching.
#[test]
pub(super) fn reading_workspace_state_is_classified_with_shell_because_it_runs_git() {
    let verdict = c("read_workspace_state");
    assert_eq!(verdict.reach, Reach::Consequence);
    assert!(
        verdict.reach.parks_under_supervision(),
        "running git under config the agent wrote must reach an operator"
    );
    assert!(
        verdict.reach.denied_under_readonly(),
        "`readonly` promises nothing runs; a config key can name a command"
    );
    assert_eq!(
        verdict.standing,
        Standing::PerCall,
        "a standing grant here would reopen the hole for its whole duration"
    );
    assert_eq!(
        verdict.reach,
        c("shell").reach,
        "it is the `shell` shape and should stay pinned to `shell`'s verdict"
    );
}

/// The feature keeps its point: the tools an agent uses to actually do work
/// in its own sandbox stay grantable, so an operator handing over a stretch
/// of autonomy is still handing over something useful.
#[test]
pub(super) fn the_agents_own_workspace_writes_stay_grantable() {
    for tool in [
        "file_write",
        "edit",
        "apply_patch",
        "csv_export",
        "memory_store",
    ] {
        let verdict = c(tool);
        assert_eq!(verdict.standing, Standing::Grantable, "`{tool}`");
        // They mutate, so `readonly` must still deny and `supervised` must
        // still park the first call.
        assert!(verdict.reach.parks_under_supervision(), "`{tool}`");
    }
}

/// Issue #559, every acceptance criterion in one place — the four verdicts
/// `ExternalRead` has to give, and the one it must not.
///
/// The three predicates are the whole of the behaviour: `Reach` is never
/// matched exhaustively outside this module, so adding a variant changes
/// nothing anywhere until one of these answers differently.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn a_composio_read_runs_under_supervision_and_is_still_denied_under_readonly() {
    use crate::policy::test_support::{COMPOSIO_READ_SLUG, composio_read_args};

    let read = consequence_of(COMPOSIO_EXECUTE, &composio_read_args());
    assert_eq!(
        read.reach,
        Reach::ExternalRead,
        "`{COMPOSIO_READ_SLUG}` is tagged `Read` in the vendored catalogue"
    );

    // 1. Under `supervised` it runs, instead of costing the operator a card.
    assert!(
        !read.reach.parks_under_supervision(),
        "reading a mailbox must not interrupt a person"
    );
    // 2. Under `readonly` it is still denied: that tier's contract is that
    //    nothing outside the company is reached at all.
    assert!(read.reach.denied_under_readonly());
    // 3. And it is NOT spend. This is the criterion that rules out reusing
    //    `Reach::Money`, whose `costs_money()` feeds the daily cap — every
    //    page of every mailbox would have counted against it.
    assert!(
        !read.reach.costs_money(),
        "a read is not billed; folding it into `Money` would bill it"
    );
    assert_ne!(read.group, EffectGroup::Spend);

    // 4. The standing answer is unchanged — it stops mattering for
    //    `supervised` now that nothing parks there, but still governs
    //    `readonly` and any tier added later.
    assert_eq!(read.standing, Standing::Grantable);
    assert_eq!(read.group, EffectGroup::Other);
}

/// The other half of #559: only the **read** branch moved.
///
/// The `send` binding is shared by the missing-key path and the non-read
/// path, so these hold structurally — but nothing stops a later edit from
/// touching that shared binding, which is the whole reason to assert them.
#[test]
pub(super) fn a_composio_send_and_every_unclassifiable_call_still_park() {
    use crate::policy::test_support::{composio_send_args, composio_unclassified_args};

    let cases: [(&str, serde_json::Value); 6] = [
        ("a catalogued send", composio_send_args()),
        ("an uncatalogued action", composio_unclassified_args()),
        (
            "an unrecognised slug in a real toolkit",
            json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" }),
        ),
        ("a non-string `tool`", json!({ "tool": 7 })),
        ("an empty slug", json!({ "tool": "" })),
        ("a missing `tool` key", json!({ "arguments": { "q": "x" } })),
    ];

    for (what, args) in cases {
        let verdict = consequence_of(COMPOSIO_EXECUTE, &args);
        assert_eq!(verdict.reach, Reach::Consequence, "{what}: {args}");
        assert!(
            verdict.reach.parks_under_supervision(),
            "{what} must still park: {args}"
        );
        assert!(verdict.reach.denied_under_readonly(), "{what}: {args}");
        assert_eq!(verdict.group, EffectGroup::Send, "{what}: {args}");
        assert_eq!(verdict.standing, Standing::PerCall, "{what}: {args}");
    }
}

/// `ExternalRead` must not leak into the spend cap.
#[test]
pub(super) fn external_reads_never_claim_the_spend_bucket() {
    let args = json!({
        "url": "https://api.github.com/repos/o/r",
        "method": "GET",
        COMPOSIO_ACTION_KEY: "GITHUB_GET_A_REPOSITORY",
    });
    let mut seen = 0;
    for tool in declared_tools() {
        let verdict = consequence_of(tool, &args);
        if verdict.reach == Reach::ExternalRead {
            seen += 1;
            assert!(!verdict.reach.costs_money(), "`{tool}` is not spend");
            assert_ne!(verdict.group, EffectGroup::Spend, "`{tool}` is not spend");
        }
    }
    assert!(seen > 0, "the walk reached no external read");
}

#[test]
pub(super) fn a_metered_read_is_allowed_under_supervision_and_denied_under_readonly() {
    let search = c("web_search");
    assert_eq!(search.reach, Reach::Money);
    assert!(!search.reach.parks_under_supervision());
    assert!(search.reach.denied_under_readonly());
    assert!(search.reach.costs_money());
    assert_eq!(search.group, EffectGroup::Spend);
    assert_eq!(search.standing, Standing::PerCall);
}

#[test]
pub(super) fn declared_tools_covers_the_table_and_the_argument_classified_tool() {
    let all: Vec<&str> = declared_tools().collect();
    assert!(all.contains(&COMPOSIO_EXECUTE));
    assert!(all.contains(&"shell"));
    // `composio_execute` is the one roster entry with no `DECLARED` row;
    // the other four shadow theirs and are counted once.
    assert_eq!(all.len(), DECLARED.len() + 1);
}

/// The two mechanisms **partition** the names the gate knows: every name
/// [`declared_tools`] yields is answered either from its arguments or from
/// the table, never neither and never ambiguously (issue #877).
///
/// This is the criterion #877 states in as many words — *"the coverage test
/// keeps saying which tools answer from arguments and which from the
/// table"*. The roster is the authority on which side a tool is on, because
/// it is the same list [`consequence_of`] dispatches through: a tool cannot
/// be graded by argument without appearing here, and appearing here is what
/// puts it on the argument side of this assertion.
#[test]
pub(super) fn the_roster_and_the_table_partition_the_known_tool_names() {
    let known: std::collections::BTreeSet<&str> = declared_tools().collect();
    let graded: std::collections::BTreeSet<&str> =
        ARGUMENT_GRADED.iter().map(|(tool, _)| *tool).collect();
    let tabled: std::collections::BTreeSet<&str> = DECLARED
        .iter()
        .map(|d| d.tool)
        .filter(|tool| !graded.contains(tool))
        .collect();

    assert!(
        graded.is_disjoint(&tabled),
        "a name cannot be answered by both mechanisms — the roster shadows \
         the table, so a shadowed row is not on the table side"
    );
    let union: std::collections::BTreeSet<&str> = graded.union(&tabled).copied().collect();
    assert_eq!(
        union, known,
        "every known tool name must sit on exactly one side of the \
         partition; if this fails, a mechanism has grown a name \
         `declared_tools` cannot see"
    );

    // And the sides say what they are, so the failure message above is
    // actionable rather than a set difference.
    for tool in &graded {
        assert!(
            argument_grader(tool).is_some(),
            "`{tool}` is on the roster but `consequence_of` would not \
             dispatch it"
        );
    }
    for tool in &tabled {
        assert!(
            argument_grader(tool).is_none(),
            "`{tool}` answers from the table but a classifier claims it too"
        );
    }
}

/// A classifier added to the roster is enumerated by [`declared_tools`]
/// **without** anyone remembering to add it there too.
///
/// This is the regression the old shape could not guard: [`declared_tools`]
/// used to `chain(once(COMPOSIO_EXECUTE))`, naming the single exception by
/// hand, so a fifth argument-graded tool with no [`DECLARED`] row would
/// have been dispatched and yet invisible to every test that walks
/// [`declared_tools`] — #877's "quietly join the coarse side". Driving the
/// derivation with a synthetic roster is the only way to assert it without
/// shipping a fake tool.
#[test]
pub(super) fn a_roster_entry_with_no_table_row_is_still_enumerated() {
    const SYNTHETIC: &[(&str, Grader)] = &[("not_a_real_tool", shell_consequence)];
    let names: Vec<&str> = tool_names(DECLARED, SYNTHETIC).collect();
    assert!(
        names.contains(&"not_a_real_tool"),
        "a roster entry with no `DECLARED` row must still be enumerated"
    );
    assert_eq!(
        names.len(),
        DECLARED.len() + 1,
        "and exactly once — the row-less entry is appended, nothing else moves"
    );
}

/// A roster entry that shadows a [`DECLARED`] row is counted **once**.
///
/// The union is what makes the partition above meaningful: a concatenation
/// would double-count the roster entries that keep fallback rows, and every
/// caller that walks [`declared_tools`] as a set — `always_approve`,
/// `judgement`, the harness roster — would silently do redundant work over
/// duplicated names.
#[test]
pub(super) fn a_roster_entry_that_shadows_a_table_row_is_enumerated_once() {
    let names: Vec<&str> = declared_tools().collect();
    for tool in ["shell", WEB_FETCH, "http_request", GIT_OPERATIONS] {
        assert_eq!(
            names.iter().filter(|name| **name == tool).count(),
            1,
            "`{tool}` holds both a roster entry and a `DECLARED` row and \
             must be enumerated once"
        );
    }
}

/// Every roster name is lower-case and appears once.
///
/// [`consequence_of`] lower-cases the incoming tool name before asking
/// [`argument_grader`], so a mixed-case entry would be an entry that never
/// fires — a classifier silently replaced by its table row, which is the
/// fail-open shape this whole cluster of issues exists to prevent. A
/// duplicate name would be a second classifier the first one shadows.
#[test]
pub(super) fn the_roster_is_lower_case_and_has_no_duplicates() {
    let mut seen = std::collections::BTreeSet::new();
    for (tool, _) in ARGUMENT_GRADED {
        assert_eq!(
            *tool,
            tool.to_ascii_lowercase(),
            "`{tool}` is matched against a lower-cased name and would never fire"
        );
        assert!(seen.insert(*tool), "`{tool}` appears twice on the roster");
    }
}

/// The declaration is matched case-insensitively, the way every other arm
/// of the gate reads a tool name.
#[test]
pub(super) fn lookup_ignores_case() {
    assert_eq!(c("SHELL").standing, Standing::PerCall);
    assert_eq!(c("Workspace_Read").reach, Reach::Nothing);
    // `composio_execute` is matched by the same lowercasing pass, so an
    // upper-cased tool name still reaches the argument classifier rather
    // than falling through to the undeclared heuristics.
    assert_eq!(
        consequence_of("COMPOSIO_EXECUTE", &json!({ "tool": "GMAIL_SEND_EMAIL" })).group,
        EffectGroup::Send
    );
    #[cfg(feature = "openhuman")]
    assert_eq!(
        consequence_of(
            "COMPOSIO_EXECUTE",
            &json!({ "tool": "github_list_branches" })
        )
        .standing,
        Standing::Grantable,
        "the curated lookup is case-insensitive on the slug too"
    );
}
