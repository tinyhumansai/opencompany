use super::*;

// Issue #470: the `composio_execute` fixtures are built here, from the same
// key the classifier reads, so a call in a test reaches the same catalogue
// lookup a call in production does.
use super::policy_test_helpers_tests::*;

// -----------------------------------------------------------------------
// Standing grants (issue #374)
// -----------------------------------------------------------------------
//
// These tests granted a standing scope on `workspace_write` until issue
// #444. That tool was never a fair fixture: it is grantable only under the
// rule that read grantability off a name's vocabulary, and the parking side
// of this same gate has always refused to exempt it because it overwrites
// guidance the operator wrote. The two halves of one gate disagreed about
// one tool, and the tests pinned the half that was wrong. `file_write` is
// the honest stand-in — it mutates, so it still parks the first time, but
// what it mutates is the agent's own sandboxed workspace.

/// The issue in one test: the same tool, called repeatedly with different
/// arguments, stops asking.
#[tokio::test]
async fn a_standing_grant_admits_repeat_calls_with_any_arguments() {
    let (p, grants) = granting_policy("supervised", &[], "ops");
    grants.grant_standing(standing("ops", "file_write", far_future()));

    for args in [
        serde_json::json!({ "path": "notes/a.md", "body": "one" }),
        serde_json::json!({ "path": "notes/b.md", "body": "two" }),
        serde_json::json!({}),
    ] {
        assert_eq!(
            p.check(&request("file_write", args.clone())).await,
            ToolPolicyDecision::Allow,
            "a standing grant admits any arguments: {args}"
        );
    }
    assert_eq!(
        grants.standing_count(),
        1,
        "using a standing grant must not spend it"
    );
}

/// An expired standing grant is refused at redemption, not merely swept.
///
/// The sweep runs on the scheduler's maintenance tick; between two ticks a
/// lapsed grant would otherwise keep admitting calls, and "for one hour" has
/// to mean one hour.
#[tokio::test]
async fn an_expired_standing_grant_re_parks() {
    in_cycle(async {
        let (p, grants) = granting_policy("supervised", &[], "ops");
        // Already past — and deliberately left in the set, so this proves the
        // redemption check rather than the sweep.
        grants.grant_standing(standing("ops", "file_write", 1));

        assert!(matches!(
            p.check(&request("file_write", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(grants.standing_count(), 1, "the sweep did not run here");
    })
    .await;
}

#[tokio::test]
async fn a_standing_grant_is_scoped_to_its_agent_and_tool() {
    in_cycle(async {
        let (p, grants) = granting_policy("supervised", &[], "marketing");
        // Granted to a different teammate.
        grants.grant_standing(standing("ops", "file_write", far_future()));
        assert!(matches!(
            p.check(&request("file_write", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));

        let (p, grants) = granting_policy("supervised", &[], "ops");
        grants.grant_standing(standing("ops", "file_write", far_future()));
        // A different tool for the right teammate.
        assert!(matches!(
            p.check(&request("send_email", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    })
    .await;
}

/// The single-use grant burns first, even when a standing grant would also
/// have admitted the call.
///
/// Ordering, not coincidence. If the standing arm ran first the operator's
/// one-off approval would sit unredeemed until its TTL and then be announced
/// as "the agent didn't act within 15 minutes" — a notice about work that
/// had already happened, which is worse than no notice at all.
#[tokio::test]
async fn the_single_use_grant_is_consumed_first() {
    let (p, grants) = granting_policy("supervised", &[], "ops");
    let args = serde_json::json!({ "path": "notes/a.md" });
    grants.grant(granted("ops", "file_write", args.clone()));
    grants.grant_standing(standing("ops", "file_write", far_future()));

    assert_eq!(
        p.check(&request("file_write", args)).await,
        ToolPolicyDecision::Allow
    );
    assert_eq!(grants.live_count(), 0, "the single-use grant burned");
    assert_eq!(grants.standing_count(), 1);
    assert_eq!(
        grants.drain_consumed().len(),
        1,
        "the consumption is journaled, so no phantom expiry notice follows"
    );
}

/// A standing grant can never admit money — by placement, not by promise.
///
/// The mint side refuses to grant anything the declaration does not call
/// grantable, so no Spend-group tool can have one. This covers the other way
/// a call becomes priced, which the tool name cannot predict: a grantable
/// tool invoked with a declared amount. It must fall through to the budget
/// and mode arms and park.
#[tokio::test]
async fn a_standing_grant_refuses_a_priced_call() {
    in_cycle(async {
        let (p, grants) = granting_policy("supervised", &[], "ops");
        grants.grant_standing(standing("ops", "file_write", far_future()));

        // Same tool, same grant — the only difference is a declared amount.
        assert_eq!(
            p.check(&request("file_write", serde_json::json!({ "path": "a" })))
                .await,
            ToolPolicyDecision::Allow
        );
        assert!(
            matches!(
                p.check(&request(
                    "file_write",
                    serde_json::json!({ "path": "a", "amount_usd": 25.0 })
                ))
                .await,
                ToolPolicyDecision::RequireApproval { .. }
            ),
            "a declared amount must park even under a standing grant"
        );

        // And the metered read, which is priced without declaring anything.
        let (p, grants) = granting_policy("supervised", &[], "ops");
        grants.grant_standing(standing(
            "ops",
            crate::harness::search::WEB_SEARCH_TOOL,
            far_future(),
        ));
        assert_ne!(
            p.check(&request(
                crate::harness::search::WEB_SEARCH_TOOL,
                serde_json::json!({ "query": "x" })
            ))
            .await,
            ToolPolicyDecision::Deny {
                reason: String::new()
            },
            "sanity: this asserts the arm was not reached, not the tier's answer"
        );
    })
    .await;
}

/// `readonly` outranks a standing grant, and leaves it intact.
///
/// Same argument as the single-use case: the brake is the emergency stop,
/// not a question, and consent does not survive it. But the grant is not
/// destroyed — the call never ran, so the operator's permission is still
/// there when the brake comes off.
#[tokio::test]
async fn readonly_outranks_a_standing_grant_and_leaves_it_intact() {
    let (p, grants) = granting_policy("readonly", &[], "ops");
    grants.grant_standing(standing("ops", "file_write", far_future()));

    assert!(matches!(
        p.check(&request("file_write", serde_json::json!({}))).await,
        ToolPolicyDecision::Deny { .. }
    ));
    assert_eq!(
        grants.standing_count(),
        1,
        "a denied call must not destroy the permission it never used"
    );
}

#[tokio::test]
async fn an_unbound_policy_ignores_standing_grants_entirely() {
    in_cycle(async {
        let queue = ApprovalRequestQueue::default();
        let grants = queue.grants();
        let p = policy("supervised", &[], None).with_requests(queue);
        grants.grant_standing(standing("ops", "send_email", far_future()));

        assert!(matches!(
            p.check(&request("send_email", serde_json::json!({}))).await,
            ToolPolicyDecision::RequireApproval { .. }
        ));
    })
    .await;
}

/// What may be granted standing is decided by what a tool can **reach**,
/// never by the vocabulary of its name (issue #444).
///
/// The list below is the before/after of the boundary. Every tool in the
/// first group used to be grantable — `shell` and `web_fetch` and
/// `mcp_registry_tool_call` because their names carry no consequence word,
/// and `workspace_write` in flat contradiction of the parking side of this
/// same gate, which has always refused to exempt it on the grounds that it
/// overwrites operator-owned guidance. An operator on staging really did
/// get a standing grant on running arbitrary terminal commands while being
/// refused one on reading a repository.
#[test]
fn what_may_be_granted_standing_is_what_a_tool_can_reach() {
    let args = serde_json::json!({});
    for tool in [
        // Arbitrary code, arbitrary address, operator-owned guidance.
        "shell",
        "http_request",
        "curl",
        "web_fetch",
        "workspace_write",
        "workspace_create",
        "workspace_delete",
        "workspace_rename",
        // Anything a remote server chooses to advertise.
        "mcp_registry_tool_call",
        "mcp_call_tool",
        // Third-party source and diffs, fetched under the operator's
        // credential (issue #245).
        // Named consequences, unchanged.
        "composio_authorize",
        "pay_invoice",
        "transfer_funds",
        "send_email",
        "media_generate_image",
        crate::harness::search::WEB_SEARCH_TOOL,
        "publish_post",
        "contract_accept",
        "handle_register",
        "filing_submit",
        "deploy_site",
        // A tool nobody has classified. Landing in the residual bucket no
        // longer confers the longest permission available.
        "some_tool_nobody_declared",
    ] {
        assert!(
            !grantable(tool, &args),
            "{tool} can reach further than a standing grant can describe"
        );
    }
    // The feature keeps its point: writes confined to the agent's own
    // sandbox stay grantable, so a stretch of unattended autonomy is still
    // worth granting.
    for tool in ["file_write", "edit", "apply_patch", "memory_store"] {
        assert!(grantable(tool, &args), "{tool} stays grantable");
    }
}

/// Issue #441's headline, at the gate rather than at the card: the same
/// tool, two verdicts, decided by the action in the arguments.
#[test]
fn a_composio_read_may_be_granted_standing_and_a_send_may_not() {
    assert!(grantable(
        "composio_execute",
        &serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" })
    ));
    assert!(!grantable(
        "composio_execute",
        &serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" })
    ));
    // The cautious fallback: an action the provider catalogue does not name
    // is a send, so it neither loses its per-call decision nor its `Send`
    // label on the card.
    assert!(!grantable(
        "composio_execute",
        &serde_json::json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" })
    ));
    assert_eq!(
        classify_group(
            "composio_execute",
            &serde_json::json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" })
        ),
        EffectGroup::Send
    );
}

/// **Issue #1818, end to end at the gate.** The live evidence from the
/// issue: an agent's `composio_execute` to fetch GitHub issues was blocked
/// under `opencompany-approval` — *"leaves the company or spends money"* —
/// for what is a read.
///
/// `GITHUB_ISSUES_LIST_FOR_REPO` is Composio's own spelling of the
/// operation the curated catalogue calls `GITHUB_LIST_REPOSITORY_ISSUES`.
/// The catalogue miss used to make it a `Send`, which parks under both
/// `supervised` and `auto` and — being `PerCall` — could not be unblocked
/// by granting standing either, so the desk stopped for good.
///
/// Both unattended tiers are asserted, not just `auto`: the symptom in the
/// issue is a park, and a park is what `supervised` does too. `readonly` is
/// asserted from the other side — it is the one tier whose promise is that
/// the desk reaches into nobody's account, and a drifted slug is no reason
/// to break it.
#[tokio::test]
#[cfg(feature = "openhuman")]
async fn a_drifted_composio_read_no_longer_parks_as_spend() {
    in_cycle(async {
        let drifted = || serde_json::json!({ "tool": "GITHUB_ISSUES_LIST_FOR_REPO" });
        for mode in ["supervised", "auto"] {
            let p = policy(mode, &[], None).with_agent("ops");
            assert_eq!(
                p.check(&request("composio_execute", drifted())).await,
                ToolPolicyDecision::Allow,
                "under `{mode}` a GitHub issue read must run, not park behind a card \
             that says it spends money"
            );
        }
        // The card never says spend, whatever tier is asking.
        assert_eq!(
            classify_group("composio_execute", &drifted()),
            EffectGroup::Other,
        );
        // …and the two boundaries the fix does not move. `readonly` still
        // refuses: this reaches a third party's account with the company's
        // credential, which is exactly what that tier promises not to do.
        let ro = policy("readonly", &[], None).with_agent("ops");
        assert!(matches!(
            ro.check(&request("composio_execute", drifted())).await,
            ToolPolicyDecision::Deny { .. }
        ));
        // And an inferred read is not mintable: a verb is evidence, and a
        // standing grant outlives the call it was cut from.
        assert!(!grantable("composio_execute", &drifted()));
        // The control, in the same tiers: a send that merely misses the
        // catalogue is still a send. `GITHUB_INVENT_A_NEW_VERB` names no verb
        // this layer knows, so nothing about #1818 rescues it.
        for mode in ["supervised", "auto"] {
            let p = policy(mode, &[], None).with_agent("ops");
            assert!(
                matches!(
                    p.check(&request(
                        "composio_execute",
                        serde_json::json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" })
                    ))
                    .await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "under `{mode}` an action nobody has classified must still park"
            );
        }
    })
    .await;
}

/// The grantability answer and the parking answer are read from one
/// declaration, so an effect the policy projects and the effect the mint
/// path inspects can never disagree — for every tool, and for both shapes
/// of a Composio call.
#[test]
fn the_projected_effect_and_the_mint_rule_agree_about_every_tool() {
    let p = policy("supervised", &[], None).with_agent("ops");
    let cases: Vec<(&str, serde_json::Value)> = crate::policy::consequence::declared_tools()
        .map(|t| (t, serde_json::json!({})))
        .chain([
            (
                "composio_execute",
                serde_json::json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" }),
            ),
            (
                "composio_execute",
                serde_json::json!({ "tool": "GMAIL_SEND_EMAIL" }),
            ),
            ("some_tool_nobody_declared", serde_json::json!({})),
        ])
        .collect();
    for (tool, args) in cases {
        let effect = p.effect_for(tool, &args);
        assert_eq!(
            effect.may_be_granted_standing(),
            grantable(tool, &args),
            "the parked effect for `{tool}` disagrees with the declaration it came from"
        );
        assert_eq!(
            effect.group,
            classify_group(tool, &args),
            "the parked effect's group for `{tool}` is not the one the classifier gave"
        );
    }
}

/// The arm `is_priced_call` dropped, pinned from the other side.
///
/// Removing the metered-read arm was safe only because the one
/// `Reach::Money` tool is also `EffectGroup::Spend`. If a future tool is
/// billed without being classified as spend, it would slip the daily cap
/// silently — so this fails instead.
#[test]
fn web_search_is_still_a_priced_call() {
    let args = serde_json::json!({});
    assert!(ApprovalPolicy::is_priced_call(
        crate::harness::search::WEB_SEARCH_TOOL,
        &args,
        None
    ));
    for tool in crate::policy::consequence::declared_tools() {
        let verdict = crate::policy::consequence_of(tool, &args);
        if verdict.reach.costs_money() {
            assert_eq!(
                verdict.group,
                EffectGroup::Spend,
                "`{tool}` is billed but is not classified as spend, so the daily \
                 budget arm would never see it"
            );
        }
    }
}

/// Issue #443: the persona instructs every agent to call these rather than
/// answer a capability question from memory, and under the DEFAULT
/// supervised mode that instruction used to cost an operator approval to
/// follow. Calling *through* a server still parks.
#[tokio::test]
async fn listing_mcp_servers_and_tools_runs_without_asking() {
    in_cycle(async {
        let p = policy("supervised", &[], None);
        for tool in ["mcp_list_tools", "mcp_registry_list_tools"] {
            assert_eq!(
                p.check(&request(tool, serde_json::json!({}))).await,
                ToolPolicyDecision::Allow,
                "`{tool}` reads local registration state and reaches nothing"
            );
        }
        for tool in ["mcp_call_tool", "mcp_registry_tool_call"] {
            assert!(
                matches!(
                    p.check(&request(tool, serde_json::json!({}))).await,
                    ToolPolicyDecision::RequireApproval { .. }
                ),
                "`{tool}` can perform any effect the remote server advertises"
            );
        }
    })
    .await;
}
