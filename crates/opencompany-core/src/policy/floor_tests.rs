use super::*;
use crate::policy::consequence::{Consequence, Standing};
use crate::policy::test_support::composio_args;
use serde_json::json;

fn verdict(tool: &str) -> FloorVerdict {
    evaluate(tool, &json!({}), None)
}

#[test]
fn the_floor_stops_every_irreversible_declared_class() {
    for tool in [
        "chargebee_send_invoice",
        "hosting_launch_site",
        "composio_authorize",
    ] {
        assert!(
            verdict(tool).requires_human(),
            "{tool} commits the company and must reach a person"
        );
    }
}

/// The maintainer-facing proof that this is not what #1925 deleted.
///
/// #1925 removed interruption keyed on which tool was reached for. Every
/// name below is exactly that shape — a tool whose *mechanism* is broad —
/// and the floor is silent on all of them.
#[test]
fn the_floor_is_silent_on_mechanism_alone() {
    for tool in [
        "shell",
        "curl",
        "http_request",
        "git_operations",
        "workspace_write",
    ] {
        assert_eq!(
            verdict(tool),
            FloorVerdict::Silent,
            "{tool} is broad in mechanism, not a commitment — the floor must not speak"
        );
    }
}

#[test]
fn an_undeclared_tool_is_not_the_floors_business() {
    assert_eq!(
        evaluate("some_tool_nobody_declared", &json!({}), None),
        FloorVerdict::Silent,
        "fail-closed on an unknown tool is judge's mechanism arm, deliberately not the floor's"
    );
}

#[test]
fn a_metered_read_is_not_a_commitment() {
    assert_eq!(
        verdict("web_search"),
        FloorVerdict::Silent,
        "declared Spend, but its reach is Money: the money buys the call, nothing leaves"
    );
}

/// A Composio call is classified from its action, and the curated
/// catalogue that does the classifying is compiled only under `openhuman`.
///
/// Both builds are pinned, in separate tests, because the difference is a
/// deliberate seam rather than an accident: without the catalogue every
/// action is read as a send, which is the cautious direction and the answer
/// a default build must keep giving. Asserting only the catalogued answer
/// is what made this test green locally and red on a non-`openhuman` lane.
#[test]
#[cfg(feature = "openhuman")]
fn a_composio_read_is_silent_and_a_composio_send_is_not() {
    assert_eq!(
        evaluate(
            crate::policy::consequence::COMPOSIO_EXECUTE,
            &composio_args("GITHUB_LIST_REPOSITORY_ISSUES"),
            None,
        ),
        FloorVerdict::Silent
    );
    assert!(
        evaluate(
            crate::policy::consequence::COMPOSIO_EXECUTE,
            &composio_args("GMAIL_SEND_EMAIL"),
            None,
        )
        .requires_human(),
        "sending mail from the company's account is the archetype of a commitment"
    );
}

/// Without the curated catalogue there is nothing to tell a read from a
/// send, so every Composio action is a send — including the read.
///
/// Pinned rather than tolerated: this is the build a self-hoster gets, and
/// "the floor is stricter where it can see less" has to be a stated
/// property, not an artefact nobody checked.
#[test]
#[cfg(not(feature = "openhuman"))]
fn without_the_catalogue_every_composio_action_is_a_commitment() {
    for slug in ["GITHUB_LIST_REPOSITORY_ISSUES", "GMAIL_SEND_EMAIL"] {
        assert!(
            evaluate(
                crate::policy::consequence::COMPOSIO_EXECUTE,
                &composio_args(slug),
                None,
            )
            .requires_human(),
            "{slug}: with no catalogue compiled in, cautious is the only honest answer"
        );
    }
}

#[test]
fn an_amount_under_the_cap_does_not_stop_and_at_the_cap_does() {
    let args = json!({ AMOUNT_KEY: 10.0 });
    assert_eq!(
        evaluate("file_write", &args, Some(25.0)),
        FloorVerdict::Silent
    );
    assert_eq!(
        evaluate("file_write", &args, Some(10.0)),
        FloorVerdict::MoneyLeaves,
        "at the cap is over the allowance, not under it"
    );
}

#[test]
fn without_a_cap_any_declared_amount_stops() {
    assert_eq!(
        evaluate("file_write", &json!({ AMOUNT_KEY: 0.01 }), None),
        FloorVerdict::MoneyLeaves,
        "this is the caller judge passes, and it must keep judge's answer"
    );
}

/// `ApprovalPolicy::amount_usd` (`src/harness/built_in/policy.rs`) reads
/// both `amount_usd` and `amount`; this reader has to recognise the same
/// two keys or the shadow measurement undercounts every call that
/// declares money under the alias.
#[test]
fn the_amount_alias_is_read_too() {
    assert_eq!(
        declared_amount_usd(&json!({ "amount": 12.5 })),
        Some(12.5),
        "amount is the alias ApprovalPolicy::amount_usd also accepts"
    );
    assert_eq!(
        declared_amount_usd(&json!({ AMOUNT_KEY: 5.0, "amount": 99.0 })),
        Some(5.0),
        "amount_usd outranks amount when a call declares both"
    );
    assert_eq!(
        evaluate("file_write", &json!({ "amount": 0.01 }), None),
        FloorVerdict::MoneyLeaves,
        "evaluate must see money declared under the alias too"
    );
}

#[test]
fn the_deferred_publish_is_silent_but_countable() {
    assert_eq!(verdict("publish_artifact"), FloorVerdict::Silent);
    assert_eq!(
        deferred_group("publish_artifact", &json!({})),
        Some(EffectGroup::Publish),
        "#658's carve-out has to be measurable, or the ruling cannot be revisited with evidence"
    );
    assert_eq!(deferred_group("shell", &json!({})), None);
}

/// The floor and the standing-grant set must not overlap.
///
/// A grantable tool is one an operator may hand a teammate for a week
/// unattended. A floor tool is one that must reach a person every time.
/// A tool in both would mean a grant that the floor then re-parks — the
/// approval-never-authorises-anything failure the single-use grant arm
/// already argues against in `ApprovalPolicy::check`.
#[test]
fn no_declared_tool_is_both_floor_covered_and_standing_grantable() {
    for tool in declared_tools() {
        let Consequence { standing, .. } = consequence_of(tool, &json!({}));
        if standing == Standing::Grantable {
            assert_eq!(
                evaluate(tool, &json!({}), None),
                FloorVerdict::Silent,
                "{tool} may run unattended for a week AND commits the company — pick one"
            );
        }
    }
}

#[test]
fn the_reason_word_is_stable_for_every_group() {
    for group in [
        EffectGroup::Spend,
        EffectGroup::Send,
        EffectGroup::Sign,
        EffectGroup::Publish,
        EffectGroup::Hire,
        EffectGroup::Identity,
        EffectGroup::Other,
    ] {
        assert!(!FloorVerdict::Irreversible(group).reason_word().is_empty());
    }
    assert_eq!(FloorVerdict::Silent.reason_word(), "silent");
    assert_eq!(FloorVerdict::MoneyLeaves.reason_word(), "money_leaves");
}
