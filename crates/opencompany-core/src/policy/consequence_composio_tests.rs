use super::consequence_hosting_tests::*;
use super::*;
use serde_json::json;

/// The `auto` line, named tool by tool and taken from the whole table
/// rather than a sample (issue #560).
///
/// [`Consequence::parks_under_auto`] is easy to check as a predicate; what
/// an operator actually feels is *which tools* stopped asking. And since
/// #560, [`Standing::Grantable`] decides two things at once — may be
/// delegated to one teammate, **and** runs unattended for everyone under
/// `auto` — so an edit loosening one tool for a delegation reason moves it
/// across this line as a side effect.
///
/// This walks [`declared_tools`], so a tool joining or leaving the
/// unattended set fails here and has to be named deliberately. The
/// predicate test alone would not notice.
#[test]
pub(super) fn the_auto_tier_line_is_pinned_tool_by_tool() {
    // The whole of what `auto` changes: parks for an operator under
    // `supervised`, runs unattended under `auto`. Every entry is the
    // agent's own sandbox or this company's own memory — nothing here
    // leaves the building or spends money.
    const MOVED_BY_AUTO: &[&str] = &[
        "apply_patch",
        "csv_export",
        "edit",
        "file_write",
        "memory_store",
        // Issue #903, and the one entry that is not the agent's private
        // sandbox: it writes into the company's shared workspace. Declared
        // deliberately. A publish still reaches no counterparty and no
        // address, and the artifact chain versions it, so the company can
        // undo it alone — the two properties every other name here has.
        // What it buys is that a finished deliverable reaches the operator
        // without a per-file decision, which is the whole point of `auto`.
        "publish_artifact",
    ];

    let crossers = |args: &serde_json::Value| {
        let mut moved: Vec<&str> = declared_tools()
            .filter(|tool| {
                let verdict = consequence_of(tool, args);
                verdict.reach.parks_under_supervision() && !verdict.parks_under_auto()
            })
            .collect();
        moved.sort_unstable();
        moved
    };

    assert_eq!(
        crossers(&json!({})),
        MOVED_BY_AUTO,
        "a tool crossed the `auto` line. If that is intended, say so here — \
         `Standing::Grantable` now also means 'runs unattended for every agent \
         while the company sits in auto', which is wider than the standing \
         grant the field is named for"
    );

    // The same walk with arguments (issue #673). Two tools are classified
    // from their arguments rather than their name, so the empty-args walk
    // above cannot see the verdict they actually produce in service — a
    // `web_fetch` reading a real URL is the grantable shape, and the bare
    // name is not. Without this the line would be pinned only for the tools
    // whose classification the walk happens to be able to reach, and a
    // `web_fetch` loosened to `Standing::Grantable` would cross this line
    // unobserved.
    assert_eq!(
        crossers(&json!({
            WEB_FETCH_URL_KEY: "https://docs.rs/serde",
            COMPOSIO_ACTION_KEY: "GITHUB_GET_A_REPOSITORY",
        })),
        MOVED_BY_AUTO,
        "a tool crossed the `auto` line once its arguments were read"
    );

    // The other direction, spelled out: the tools an operator would be
    // most alarmed to find running unattended still park.
    for tool in [
        "shell",
        "http_request",
        "git_operations",
        "workspace_write",
        "workspace_delete",
        "workspace_rename",
        "media_generate_image",
        "media_generate_video",
        "mcp_call_tool",
        "run_workflow",
        // Issue #661 (M7): removing a workflow takes its whole revision
        // history with it, so there is nothing to restore afterwards. Its
        // read and update siblings deliberately do NOT park (see `DECLARED`)
        // — naming the one that does is how that split stays a decision.
        "delete_workflow",
        "some_tool_nobody_declared",
    ] {
        assert!(
            c(tool).parks_under_auto(),
            "`{tool}` leaves the company, spends money, or cannot be seen into — \
             it must still park under auto"
        );
    }

    // And the boundary `auto` deliberately does not draw: a billed read is
    // not a park. `web_search` runs under `supervised` because openhuman
    // resolves a `RequireApproval` inline — a parked search never happens —
    // and `auto` must not be stricter than the tier it replaces. The daily
    // cap is what holds spend.
    assert!(!c("web_search").parks_under_auto());
    assert!(c("web_search").reach.costs_money());

    // The other boundary `auto` deliberately does not draw (issue #903):
    // handing a finished file to the operator. `publish_artifact` changes
    // state, so it keeps `Reach::Consequence` and still parks under
    // `supervised` — but it reaches no counterparty and no address, writes
    // only into the company's own workspace and artifact chain, and is
    // versioned, so it is reversible by the company alone. Parking it under
    // `auto` made every deliverable wait on a human: one 9-node pipeline
    // run generated 15 of these.
    assert!(
        !c("publish_artifact").parks_under_auto(),
        "handing a file to the operator does not leave the company"
    );
    assert!(
        c("publish_artifact").reach.parks_under_supervision(),
        "a supervised desk must still see a publish before it lands"
    );
    assert!(
        !c("publish_artifact").reach.costs_money(),
        "a publish is not a spend, so the daily cap must not bill for it"
    );
}

/// The argument-classified half of the same line: a Composio read runs
/// unattended under `auto`, a send does not — and the cautious fallback
/// keeps an unclassified action on the parking side.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn the_auto_line_reads_composio_arguments_not_the_tool_name() {
    let auto =
        |slug: &str| consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": slug })).parks_under_auto();
    assert!(!auto("GITHUB_LIST_PULL_REQUESTS"), "a catalogue read runs");
    assert!(auto("GMAIL_SEND_EMAIL"), "a send still parks");
    assert!(
        auto("GITHUB_INVENT_A_NEW_VERB"),
        "an action nobody has classified is a send, in this tier too"
    );
}

#[test]
pub(super) fn the_table_names_each_tool_once() {
    let mut seen: Vec<&str> = DECLARED.iter().map(|d| d.tool).collect();
    let before = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(before, seen.len(), "a tool is declared twice: {seen:?}");
    for entry in DECLARED {
        assert_eq!(
            entry.tool,
            entry.tool.to_ascii_lowercase(),
            "declarations are matched lowercased, so `{}` could never be found",
            entry.tool
        );
        assert_ne!(
            entry.tool, COMPOSIO_EXECUTE,
            "`composio_execute` is classified from its arguments, not the table"
        );
    }
}

/// Issue #444's headline: the three broadest capabilities in the system
/// were grantable for up to a week because their names carry no
/// consequence word. They are named tools now, and named tools are
/// classified by what they reach.
#[test]
pub(super) fn arbitrary_code_addresses_and_operator_guidance_are_never_grantable() {
    for tool in [
        "shell",
        "http_request",
        "curl",
        "web_fetch",
        "workspace_create",
        "workspace_write",
        "workspace_delete",
        "workspace_rename",
        "git_operations",
        "run_workflow",
        "mcp_call_tool",
        "mcp_registry_tool_call",
    ] {
        assert_eq!(
            c(tool).standing,
            Standing::PerCall,
            "`{tool}` can reach further than a standing grant can honestly describe"
        );
    }
}

/// The other half of #444: a tool nobody has classified must not inherit
/// the longest permission available just by landing in the residual bucket.
#[test]
pub(super) fn an_undeclared_tool_is_never_grantable() {
    assert_eq!(c("some_tool_nobody_declared").standing, Standing::PerCall);
    // Including one that reads — not grantable is about standing, not about
    // whether it parks.
    let read = c("list_something_undeclared");
    assert_eq!(read.reach, Reach::Nothing);
    assert_eq!(read.standing, Standing::PerCall);
}

/// Issue #441: the consequence of a Composio call is a property of the
/// action, not of the one tool name every action arrives under.
///
/// Gated on the harness feature because the read verdict comes from the
/// vendored provider catalogue, which is only linked in there — the
/// default build's cautious fallback is pinned separately by
/// [`without_the_catalogue_every_composio_action_is_a_send`].
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn a_composio_read_is_grantable_and_a_send_is_not() {
    let read = consequence_of(
        COMPOSIO_EXECUTE,
        &json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" }),
    );
    assert_eq!(read.group, EffectGroup::Other);
    assert_eq!(read.standing, Standing::Grantable);
    // …and since issue #559 it does not park: it reaches GitHub, so
    // `readonly` still denies it, but it changes nothing and costs nothing,
    // so `supervised` runs it.
    assert_eq!(read.reach, Reach::ExternalRead);

    let send = consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": "GMAIL_SEND_EMAIL" }));
    assert_eq!(send.group, EffectGroup::Send);
    assert_eq!(send.standing, Standing::PerCall);
    assert_eq!(send.reach, Reach::Consequence);
}

/// The cautious direction, four ways: an action whose slug says nothing
/// this module recognises, a missing slug, a slug of the wrong type, and
/// arguments with no slug at all.
///
/// Narrowed by issue #1818, and the narrowing is the point. `..._LIST_...`
/// left this list because a slug that names a read verb is no longer
/// "unclassifiable" — see
/// [`a_drifted_read_runs_instead_of_parking_as_spend`]. What stayed is
/// every shape that offers no evidence either way, and for those the answer
/// is the same one it always was.
#[test]
pub(super) fn an_unrecognised_composio_action_is_a_send() {
    for args in [
        json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" }),
        json!({ "tool": "NOTAREALTOOLKIT_DO_SOMETHING" }),
        json!({ "tool": "" }),
        json!({ "tool": 7 }),
        json!({ "arguments": { "owner": "acme" } }),
        json!({}),
    ] {
        let verdict = consequence_of(COMPOSIO_EXECUTE, &args);
        assert_eq!(
            verdict.group,
            EffectGroup::Send,
            "an unclassifiable action must read as a send: {args}"
        );
        assert_eq!(verdict.standing, Standing::PerCall, "{args}");
    }
}

/// **Issue #1818, the headline.** A Composio *read* whose slug the curated
/// catalogue cannot place no longer parks under a card that says it leaves
/// the company or spends money.
///
/// `GITHUB_ISSUES_LIST_FOR_REPO` is the live evidence from the issue: the
/// same GitHub operation as the curated `GITHUB_LIST_REPOSITORY_ISSUES`,
/// under Composio's `operationId`-derived spelling. Before this it was a
/// `Send + Consequence + PerCall` — parked, labelled as spend, and
/// un-grantable, so the desk could not even be unblocked by consenting once.
///
/// The two halves that must both hold: the reach is the one a read
/// deserves, and the group never says spend.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn a_drifted_read_runs_instead_of_parking_as_spend() {
    for slug in [
        // Every slug here is absent from the curated catalogue — checked,
        // not assumed: `a_drifted_read_is_a_miss_and_its_curated_twin_is_not`
        // pins the first one as an `UncuratedAction`, and a slug that
        // quietly gained a curated entry would make this test pass for the
        // wrong reason.
        "GITHUB_ISSUES_LIST_FOR_REPO",
        "GITHUB_GET_ISSUE",
        "GITHUB_LIST_ISSUES",
        "SLACK_SEARCH_MESSAGES",
        "NOTION_SEARCH_PAGES",
    ] {
        assert!(
            matches!(
                composio_catalog_lookup(slug),
                CatalogLookup::UncuratedAction { .. } | CatalogLookup::UnknownToolkit { .. }
            ),
            "`{slug}` is curated now, so it no longer exercises the fallback — \
             pick another uncurated read"
        );
        let verdict = consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": slug }));
        assert_eq!(
            verdict.reach,
            Reach::ExternalRead,
            "`{slug}` names a read verb; it must not be priced as a send"
        );
        assert_eq!(
            verdict.group,
            EffectGroup::Other,
            "`{slug}` is a read — the card must not say spend"
        );
        assert!(
            !verdict.parks_under_auto(),
            "`{slug}` must not stall an auto desk"
        );
        assert!(
            !verdict.reach.parks_under_supervision(),
            "`{slug}` must not interrupt a supervised operator either"
        );
        // The tier that still says no, and should: `readonly` promises the
        // desk reaches into nobody's account, drifted slug or not.
        assert!(verdict.reach.denied_under_readonly(), "{slug}");
    }
}

/// The other half of #1818: an inferred read runs, but it can never be
/// minted into a standing grant.
///
/// A curated read is `Grantable` because a person classified it. A verb is
/// evidence, not a classification, and a standing grant outlives the call
/// it was cut from — so the guess gets the narrow reading, which expires
/// with the turn. `PerCall` costs nothing here precisely because
/// `ExternalRead` does not park: there is no approval to save.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn an_inferred_read_is_never_grantable() {
    let inferred = consequence_of(
        COMPOSIO_EXECUTE,
        &json!({ "tool": "GITHUB_ISSUES_LIST_FOR_REPO" }),
    );
    assert_eq!(inferred.standing, Standing::PerCall);
    let curated = consequence_of(
        COMPOSIO_EXECUTE,
        &json!({ "tool": "GITHUB_LIST_REPOSITORY_ISSUES" }),
    );
    assert_eq!(
        curated.standing,
        Standing::Grantable,
        "the curated twin keeps the grant a person's classification earned"
    );
    assert_eq!(
        inferred.reach, curated.reach,
        "they differ on standing and on nothing else — that is the whole distinction"
    );
    assert_eq!(inferred.group, curated.group);
}

/// The fallback's own table, stated rather than sampled through
/// `consequence_of` (issue #1818).
///
/// Both directions matter and they are not symmetric: a `true` here runs
/// unattended, a `false` is the pre-#1818 park an operator can still
/// approve. So the read side is checked for the shapes that must run, and
/// the send side for every shape that must not — including the two the
/// rules exist for, whole-segment matching and first-verb-wins.
#[test]
pub(super) fn the_verb_fallback_asks_for_evidence_of_a_read() {
    for slug in [
        "GITHUB_ISSUES_LIST_FOR_REPO",
        "GITHUB_LIST_REPOSITORY_ISSUES",
        "GITHUB_GET_A_PULL_REQUEST",
        "GMAIL_FETCH_EMAILS",
        "SLACK_SEARCH_MESSAGES",
        "NOTION_QUERY_DATABASE",
        "linear_list_issues",
        // Whole-segment matching: the mutating verb is a prefix of the
        // object, not the verb. A `contains` rule would send both.
        "GITHUB_LIST_STARGAZERS",
        "GMAIL_LIST_DRAFTS",
    ] {
        assert!(
            composio_slug_reads_by_verb(slug),
            "`{slug}` names a read verb and nothing that mutates"
        );
    }
    for slug in [
        "GMAIL_SEND_EMAIL",
        "GITHUB_CREATE_AN_ISSUE",
        "GITHUB_CREATE_OR_UPDATE_FILE_CONTENTS",
        "STRIPE_CREATE_A_CHARGE",
        "TWITTER_POST_TWEET",
        "GOOGLECALENDAR_QUICK_ADD",
        // No verb either list knows: absence of evidence is a send here,
        // which is the whole difference from upstream's `classify_unknown`.
        "GITHUB_INVENT_A_NEW_VERB",
        "NOTAREALTOOLKIT_DO_SOMETHING",
        "",
        "_",
        // The compound shapes a first-verb-wins rule would let through: a
        // read verb opens each one and a mutation follows it.
        "GITHUB_GET_AND_UPDATE_ISSUE",
        "GMAIL_FIND_OR_CREATE_CONTACT",
        "GMAIL_GET_AND_DELETE_THREAD",
        "GITHUB_LIST_AND_REMOVE_LABELS",
        // The same shape without the conjunction, which is why the object
        // slot gets no exemption. This one is real: a curated **write**.
        "GOOGLESHEETS_FIND_REPLACE",
        "TELEGRAM_ANSWER_CALLBACK_QUERY",
        // Curated, so the fallback never sees it — but if it did, the
        // noun `DRAFT` is indistinguishable from an elided second verb and
        // the rule takes the over-gating side.
        "GMAIL_GET_DRAFT",
    ] {
        assert!(
            !composio_slug_reads_by_verb(slug),
            "`{slug}` is not evidence of a read"
        );
    }
}

/// Same verdict, different reasons — and the reasons are now separable
/// (issue #470). A slug the catalogue cannot place is a legitimate call to
/// an unclassified action; an argument shape with no readable slug is a
/// caller bug that the send verdict would otherwise hide, which is exactly
/// how the `tool_slug` fixtures passed for as long as they did.
#[test]
pub(super) fn a_missing_action_key_is_distinguishable_from_an_unknown_action() {
    assert_eq!(
        composio_action_slug(&json!({ "tool": "NOTAREALTOOLKIT_LIST_THINGS" })),
        Ok("NOTAREALTOOLKIT_LIST_THINGS"),
        "an uncatalogued slug is still a slug — the catalogue, not this reader, \
         is what declines it"
    );
    for (args, expected) in [
        (
            json!({ "tool_slug": "GMAIL_SEND_EMAIL" }),
            ActionKeyMiss::KeyAbsent,
        ),
        (
            json!({ "arguments": { "owner": "acme" } }),
            ActionKeyMiss::KeyAbsent,
        ),
        (json!({}), ActionKeyMiss::KeyAbsent),
        (json!({ "tool": 7 }), ActionKeyMiss::NotAString),
        (json!({ "tool": null }), ActionKeyMiss::NotAString),
        (json!({ "tool": "" }), ActionKeyMiss::Empty),
        (json!({ "tool": "   " }), ActionKeyMiss::Empty),
        (json!("GMAIL_SEND_EMAIL"), ActionKeyMiss::NotAnObject),
        (json!(null), ActionKeyMiss::NotAnObject),
    ] {
        assert_eq!(composio_action_slug(&args), Err(expected), "{args}");
        // …and the verdict is unchanged by any of it: the log line is the
        // only thing that differs, so this can never loosen a decision.
        assert_eq!(
            consequence_of(COMPOSIO_EXECUTE, &args).group,
            EffectGroup::Send,
            "{args}"
        );
    }
}

/// A grant scope is read through the same reader, so a call whose slug the
/// classifier could not find cannot resolve a toolkit either — `None`, and
/// a scoped grant refuses to admit `None`.
#[test]
pub(super) fn an_unreadable_action_key_resolves_no_grant_scope() {
    for args in [
        json!({ "tool_slug": "GITHUB_LIST_PULL_REQUESTS" }),
        json!({ "tool": "" }),
        json!({ "tool": 7 }),
        json!({}),
    ] {
        assert_eq!(standing_scope_of(COMPOSIO_EXECUTE, &args), None, "{args}");
    }
}

/// The seam, pinned from the other side. Without the harness feature the
/// curated catalogue is not linked in, and the mint path still has to
/// answer the grantability question — so it answers it the cautious way,
/// for a read as much as for a send. A default build can only ever see a
/// `composio_execute` effect replayed from a journal line an openhuman
/// build wrote, so refusing the standing scope there costs an operator one
/// approve-once and never a wrong grant.
#[test]
#[cfg(not(feature = "openhuman"))]
pub(super) fn without_the_catalogue_every_composio_action_is_a_send() {
    // Including one whose verb the #1818 fallback would happily call a
    // read. The fallback is deliberately not consulted here: a build that
    // cannot place `GITHUB_LIST_PULL_REQUESTS` has not earned the right to
    // infer anything, and `CatalogueAbsent` is the arm that says so.
    for slug in [
        "GITHUB_LIST_PULL_REQUESTS",
        "GITHUB_ISSUES_LIST_FOR_REPO",
        "GMAIL_SEND_EMAIL",
    ] {
        assert_eq!(
            composio_catalog_lookup(slug),
            CatalogLookup::CatalogueAbsent,
            "`{slug}` cannot be looked up in a build with no catalogue, and the \
             record must say that rather than blaming the slug (issue #1818)"
        );
        let verdict = consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": slug }));
        assert_eq!(verdict.group, EffectGroup::Send, "{slug}");
        assert_eq!(verdict.standing, Standing::PerCall, "{slug}");
    }
}

/// The seam named from the other side (issue #1818): with the catalogue
/// linked in, no lookup may ever answer "there is no catalogue".
///
/// `CatalogueAbsent` is a fact about the binary. If it could also arise
/// from a slug, the operator-facing warning it triggers — *every* Composio
/// action over-gates in this build — would be a lie told once per stale
/// slug, and the deployment bug it exists to surface would be unfindable.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn a_catalogued_build_never_reports_the_catalogue_absent() {
    for slug in [
        "GITHUB_LIST_PULL_REQUESTS",
        "GITHUB_ISSUES_LIST_FOR_REPO",
        "GMAIL_SEND_EMAIL",
        "NOTAREALTOOLKIT_DO_SOMETHING",
        "noUnderscore",
    ] {
        assert_ne!(
            composio_catalog_lookup(slug),
            CatalogLookup::CatalogueAbsent,
            "`{slug}` was looked up against a catalogue that is present"
        );
    }
}

/// Deliberately pinned: upstream's own `classify_unknown` would call
/// `GITHUB_INVENT_A_NEW_VERB` a read (its fallback arm returns `Read` when
/// no write verb matches). We do not use it, and this is the test that says
/// so — if somebody swaps the lookup for the heuristic to "cover more
/// slugs", the unknown-is-a-send guarantee goes with it.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn we_do_not_fall_back_to_the_upstream_read_default() {
    use openhuman_core::integrations::composio::contract::scopes::{ToolScope, classify_unknown};
    assert_eq!(
        classify_unknown("GITHUB_INVENT_A_NEW_VERB"),
        ToolScope::Read,
        "upstream's fallback still defaults to read; if this changes the \
         comment above is stale, not the behaviour"
    );
    assert!(!composio_catalog_lookup("GITHUB_INVENT_A_NEW_VERB").is_read());
    // …and issue #1818's fallback did not quietly become that heuristic
    // either. It asks for a read verb; upstream asks only for the absence
    // of a write one, and this slug is the case that separates them.
    assert!(!composio_slug_reads_by_verb("GITHUB_INVENT_A_NEW_VERB"));
    assert_eq!(
        consequence_of(
            COMPOSIO_EXECUTE,
            &json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" })
        )
        .group,
        EffectGroup::Send
    );
}

/// **The safety property of the #1818 fallback, over the whole catalogue.**
///
/// The fallback only ever fires on slugs the catalogue *cannot* place, so
/// there is no direct corpus of them to test against. The curated catalogue
/// is the next best thing and it is a strong one: ~680 actions a person
/// hand-classified as `Read` / `Write` / `Admin`. Running the verb rule
/// over them measures exactly what it would do on the uncurated slugs of
/// the same shape.
///
/// The two directions are **not** symmetric, so they are asserted
/// differently:
///
/// * A `Write` or `Admin` the rule calls a read would run unattended.
///   That is the bug this test exists to prevent, and it is asserted at
///   zero. It found two real vocabulary gaps when it was written —
///   `TELEGRAM_ANSWER_CALLBACK_QUERY` (a write, whose `QUERY` is a noun)
///   and `GOOGLESHEETS_FIND_REPLACE` (a write, a find-and-replace with the
///   conjunction elided) — which is why `ANSWER` and `REPLACE` are in
///   `MUTATES`.
/// * A `Read` the rule calls a send merely parks, which is the pre-#1818
///   behaviour. So that side gets a floor rather than a zero: the point is
///   to notice a rule that has stopped rescuing anything, not to chase the
///   last slug.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn the_fallback_never_calls_a_curated_write_a_read() {
    use openhuman_core::integrations::composio::contract::catalogs::catalog_for_toolkit;
    use openhuman_core::integrations::composio::contract::scopes::{
        ToolScope, agent_ready_toolkits,
    };

    let entries: Vec<_> = agent_ready_toolkits()
        .into_iter()
        .filter_map(catalog_for_toolkit)
        .flatten()
        .collect();
    assert!(
        entries.len() > 400,
        "the vendored catalogue should be hundreds of actions, found {} — this test \
         is only worth anything if it walks a real corpus",
        entries.len()
    );

    let leaked: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.scope != ToolScope::Read)
        .filter(|entry| composio_slug_reads_by_verb(entry.slug))
        .map(|entry| entry.slug)
        .collect();
    assert!(
        leaked.is_empty(),
        "the verb fallback would run these curated writes unattended: {leaked:?}. \
         Each one names a verb `MUTATES` is missing — add it there rather than \
         narrowing the rule."
    );

    // The other direction: a floor, because over-gating is only a park.
    let reads: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.scope == ToolScope::Read)
        .map(|entry| entry.slug)
        .collect();
    let rescued = reads
        .iter()
        .filter(|slug| composio_slug_reads_by_verb(slug))
        .count();
    assert!(
        rescued * 100 >= reads.len() * 85,
        "the verb rule recognises only {rescued} of {} curated reads. It has stopped \
         rescuing the drifted reads #1818 is about — a read verb was dropped, or a \
         `MUTATES` entry is matching a noun.",
        reads.len()
    );
}

/// **Issue #754.** The catalogue miss the whole issue is about, pinned from
/// both sides of the pair it was reported with.
///
/// `GITHUB_ISSUES_LIST_FOR_REPO` and `GITHUB_LIST_REPOSITORY_ISSUES` are the
/// same GitHub operation under two naming conventions — Composio's live
/// `operationId`-derived slug and the curated descriptive one. The second is
/// classified as a read and runs; the first misses the catalogue and parks.
///
/// The miss is still a miss after issue #1818 — that is what this test is
/// for, and it is why the two layers are separate functions. #1818 changed
/// what the classifier *does* with a miss; it must not change what the
/// catalogue *reports*, or the drift signal #754 exists for would be
/// silently switched off by the fix that made drift survivable.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn a_drifted_read_is_a_miss_and_its_curated_twin_is_not() {
    assert_eq!(
        composio_catalog_lookup("GITHUB_LIST_REPOSITORY_ISSUES"),
        CatalogLookup::Curated { read: true },
        "the curated spelling is a read"
    );
    assert_eq!(
        composio_catalog_lookup("GITHUB_ISSUES_LIST_FOR_REPO"),
        CatalogLookup::UncuratedAction {
            toolkit: "github".to_string()
        },
        "the live spelling of the same operation is a catalogue MISS, and \
         naming it as such is the whole of #754 — the curated name is still \
         the thing to fix even though #1818 stopped the miss from parking"
    );
}

/// A curated **write** is not a miss, and telling them apart is what keeps
/// the signal readable (issue #754).
///
/// Both classify as a send, so a boolean cannot separate them — which is
/// exactly why the drift was invisible. If every send were reported as a
/// catalogue miss, `GMAIL_SEND_EMAIL` would drown the handful of slugs that
/// have actually drifted.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn a_curated_write_is_not_reported_as_drift() {
    assert_eq!(
        composio_catalog_lookup("GMAIL_SEND_EMAIL"),
        CatalogLookup::Curated { read: false },
        "a curated send is the gate working, not the catalogue rotting"
    );
}

/// A slug whose toolkit has no curated surface is a *different* miss from a
/// slug its toolkit has never heard of, and the record says which.
#[test]
#[cfg(feature = "openhuman")]
pub(super) fn an_unrecognised_toolkit_is_its_own_kind_of_miss() {
    assert!(matches!(
        composio_catalog_lookup("NOTAREALTOOLKIT_LIST_THINGS"),
        CatalogLookup::UnknownToolkit { .. }
    ));
}
