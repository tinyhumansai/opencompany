use super::tests_owner_setup::{FailingUserStore, Harness, graph, reached_output, record};
use super::*;

use crate::ports::UserRecord;
use crate::runtime::channel::OPERATOR_CHANNEL;

/// A suspended admin and a plain member are not the owner. Only active
/// admins are.
#[tokio::test]
async fn owner_ignores_suspended_admins_and_members() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    h.add_admin("u1", "ada@acme.test").await;
    for (id, email, role, status) in [
        (
            "u2",
            "sus@acme.test",
            UserRole::Admin,
            UserStatus::Suspended,
        ),
        ("u3", "mem@acme.test", UserRole::Member, UserStatus::Active),
    ] {
        h.users
            .upsert_user(
                &h.company,
                &UserRecord {
                    id: id.to_string(),
                    email: email.to_string(),
                    display_name: None,
                    avatar: None,
                    role,
                    status,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: 1,
                    last_seen_at_millis: None,
                    updated_at_millis: 1,
                },
            )
            .await
            .unwrap();
    }

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(h.mail.sent().len(), 1);
    assert_eq!(h.mail.sent()[0].1.to, "ada@acme.test");
}

/// With no mailbox wired, `owner` falls back to the operator: the report is
/// journaled into the responsible agent's DM and the company's admins are
/// notified. The interactive buffer is untouched.
#[tokio::test]
async fn owner_falls_back_to_a_dm_and_an_admin_notification_without_mail() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), false, true);
    h.add_admin("u1", "ada@acme.test").await;

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    assert_eq!(reports[0].target.as_deref(), Some("dm:workflow"));
    assert!(reports[0].detail.contains("no mailbox"), "{reports:?}");
    assert_eq!(reports[0].reason, DeliveryReason::OwnerFellBackNoMailbox);
    assert!(h.channel.sent().is_empty());

    let landed = h.operator_report_authors().await;
    assert_eq!(landed.len(), 1, "the report must be journaled: {landed:?}");
    let (chat_id, author, text) = &landed[0];
    assert_eq!(chat_id, "dm:workflow");
    assert_eq!(author, crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR);
    assert!(text.contains("Q3 is up 12%."), "{landed:?}");
    assert!(text.contains("Report flow"), "{landed:?}");

    let notes = h.notifications_for("u1").await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].kind, "workflow_report");
    assert_eq!(notes[0].subject.id, "report_flow");
    assert_eq!(notes[0].context.as_deref(), Some("dm:workflow"));
    assert_eq!(notes[0].audience, Some(vec!["u1".to_string()]));
    assert!(
        h.notifications_for("someone-else").await.is_empty(),
        "an owner report's notification is for the admins only"
    );
}

/// The DM is the responsible agent's: the lead of the workflow's owning
/// desk, else the orchestrator.
#[tokio::test]
async fn an_operator_report_lands_in_the_responsible_agents_dm() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), false, true);

    let mut staffed = record(&[]);
    staffed.manifest = toml::from_str(
        r#"
[company]
name = "Acme"

[policy]
mode = "full"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "analyst"
role = "Analyst"

[[group_chat]]
id = "research"
name = "Research"
members = ["analyst"]
"#,
    )
    .expect("valid manifest");

    let unowned = graph("channel", Some(OPERATOR_CHANNEL));
    let mut owned = unowned.clone();
    owned.owner_desk = Some("research".to_string());

    for (flow, run) in [(&unowned, "run-1"), (&owned, "run-2")] {
        let reports =
            deliver_outputs(Some(&h.deps), &staffed, flow, run, &reached_output(), &[]).await;
        assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    }

    let landed: Vec<String> = h
        .operator_report_authors()
        .await
        .into_iter()
        .map(|(chat_id, _, _)| chat_id)
        .collect();
    assert_eq!(landed, vec!["dm:ceo", "dm:analyst"]);
}

/// The `owner` fallback journals under a distinct author from an ordinary
/// report to the operator, so the read path can restrict exactly that row to
/// administrators. An explicit `channel: operator` destination is a workflow
/// author's deliberate choice with a general audience, and keeps the ordinary
/// `WORKFLOW_REPLY_AUTHOR` and a company-wide notification.
#[tokio::test]
async fn owner_fallback_report_is_authored_distinctly_from_an_ordinary_one() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), false, true);
    h.add_admin("u1", "ada@acme.test").await;

    deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;
    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("channel", Some(OPERATOR_CHANNEL)),
        "run-2",
        &reached_output(),
        &[],
    )
    .await;
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    assert_eq!(reports[0].target.as_deref(), Some(OPERATOR_CHANNEL));

    let authors = h.operator_report_authors().await;
    assert_eq!(authors.len(), 2, "{authors:?}");
    assert!(
        authors
            .iter()
            .any(|(_, agent_id, _)| agent_id == crate::runtime::OWNER_FALLBACK_REPORT_AUTHOR),
        "the owner fallback must be marked distinctly: {authors:?}"
    );
    assert!(
        authors
            .iter()
            .any(|(_, agent_id, _)| agent_id == crate::runtime::channel::WORKFLOW_REPLY_AUTHOR),
        "an explicit `channel: operator` destination keeps the ordinary author: {authors:?}"
    );
    let for_anyone = h.notifications_for("someone-else").await;
    assert_eq!(
        for_anyone.len(),
        1,
        "only the explicit report is company-wide: {for_anyone:?}"
    );
}

/// A company with a mailbox but no admin address also reports to the
/// operator rather than failing.
#[tokio::test]
async fn owner_falls_back_to_the_operator_when_no_admin_has_an_address() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    assert_eq!(
        reports[0].reason,
        DeliveryReason::OwnerFellBackNoAdminAddress
    );
    assert!(reports[0].detail.contains("no active admin"), "{reports:?}");
    assert!(h.mail.sent().is_empty(), "nothing should have been emailed");
    assert!(h.channel.sent().is_empty());
    let landed = h.operator_reports().await;
    assert_eq!(landed.len(), 1, "the report must be journaled: {landed:?}");
}

/// No mail, and the report cannot be journaled either: still a row —
/// `failed`, naming the gap — never silence.
#[tokio::test]
async fn owner_with_neither_mail_nor_a_journal_reports_failure() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), false, false).with_failing_events();

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Failed);
    assert_eq!(reports[0].reason, DeliveryReason::OwnerFallbackFailed);
    assert!(reports[0].detail.contains("operator"), "{reports:?}");
}

// --- owner: standing admin invites (issue #661 / M8) ---------------------

/// **The M8 headline.** A fresh platform-provisioned tenant has nobody in
/// its manifest and nobody in the user store yet, but the platform injected
/// a bootstrap admin. An `owner` report must reach that address — not fall
/// back to the operator channel, which is the one human who could act on it
/// never hearing about it.
#[tokio::test]
async fn owner_emails_the_standing_bootstrap_admin_on_a_fresh_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true).with_bootstrap_admin("founder@acme.test");
    // No admins in the store, no `[users] admins` in the manifest.

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    assert_eq!(reports[0].reason, DeliveryReason::OwnerEmailed);
    assert_eq!(reports[0].target.as_deref(), Some("founder@acme.test"));
    assert_eq!(h.mail.sent().len(), 1);
    assert_eq!(h.mail.sent()[0].1.to, "founder@acme.test");
    // The operator channel must be untouched — the whole bug is that the
    // report fell back to it.
    assert!(
        h.channel.sent().is_empty(),
        "the standing admin was mailed, so nothing goes to the operator channel"
    );
    // The send is mirrored into the inbox as outbound, and journaled.
    let outbound: Vec<_> = h
        .inbox_messages()
        .await
        .into_iter()
        .filter(|m| m.outbound)
        .collect();
    assert_eq!(outbound.len(), 1, "the send must leave an audit record");
    let journaled = h.journaled_deliveries().await;
    assert_eq!(journaled.len(), 1, "{journaled:?}");
}

/// A manifest `[users] admins` entry is a standing invite too, and is mailed
/// the same way — even before that person has ever signed in.
#[tokio::test]
async fn owner_emails_a_manifest_admin_standing_invite() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    let mut rec = record(&[]);
    rec.manifest = Harness::manifest_with_admins(&["grace@acme.test"]);

    let reports = deliver_outputs(
        Some(&h.deps),
        &rec,
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    assert_eq!(reports[0].reason, DeliveryReason::OwnerEmailed);
    assert_eq!(h.mail.sent().len(), 1);
    assert_eq!(h.mail.sent()[0].1.to, "grace@acme.test");
}

/// **User-record-wins.** A bootstrap admin who has since signed in and been
/// *suspended* is not mailed through the leftover standing invite: their
/// record wins, and a suspended admin is not an active one. `owner` then has
/// no address to email and falls back to the durable operator channel with
/// the M8 wording — a real delivery (issue #1757), not the failure it once
/// reported.
#[tokio::test]
async fn owner_does_not_email_a_suspended_bootstrap_admin() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true).with_bootstrap_admin("founder@acme.test");
    // The bootstrap admin signed in, then was suspended: a record exists.
    h.users
        .upsert_user(
            &h.company,
            &UserRecord {
                id: "founder".to_string(),
                email: "founder@acme.test".to_string(),
                display_name: None,
                avatar: None,
                role: UserRole::Admin,
                status: UserStatus::Suspended,
                password_hash: None,
                must_change_password: false,
                created_at_millis: 1,
                last_seen_at_millis: None,
                updated_at_millis: 1,
            },
        )
        .await
        .unwrap();

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    assert_eq!(
        reports[0].reason,
        DeliveryReason::OwnerFellBackNoAdminAddress
    );
    assert!(
        reports[0].detail.contains("standing admin invite"),
        "the fallback wording must name standing invites now: {reports:?}"
    );
    assert!(
        h.mail.sent().is_empty(),
        "a suspended admin must not be mailed, invite or not"
    );
    assert!(
        h.channel.sent().is_empty(),
        "the interactive operator buffer is not delivery"
    );
    assert_eq!(h.operator_reports().await.len(), 1);
}

/// **Dedupe.** An address named both as an active admin and as the bootstrap
/// admin is one person, and is mailed exactly once.
#[tokio::test]
async fn owner_dedupes_an_active_admin_that_is_also_the_bootstrap_admin() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true).with_bootstrap_admin("ada@acme.test");
    h.add_admin("u1", "ada@acme.test").await;

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "one recipient, one row: {reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent);
    assert_eq!(h.mail.sent().len(), 1, "mailed once, not twice");
    assert_eq!(h.mail.sent()[0].1.to, "ada@acme.test");
}

/// A manifest admin address is normalized the same way the login path
/// normalizes it, so `Grace@ACME.test` and `grace@acme.test` are one address
/// — the send goes to the normalized form.
#[tokio::test]
async fn owner_normalizes_a_manifest_admin_address() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    let mut rec = record(&[]);
    rec.manifest = Harness::manifest_with_admins(&["Grace@ACME.test"]);

    let reports = deliver_outputs(
        Some(&h.deps),
        &rec,
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent, "{reports:?}");
    assert_eq!(h.mail.sent()[0].1.to, "grace@acme.test");
}

/// **Store-error stance (the M8 bug's worst case).** When the user store
/// cannot be read, the standing invites are mailed anyway — dropping the only
/// humans the company is known to have back to the operator channel is
/// exactly the silent drop M8 fixes.
#[tokio::test]
async fn owner_still_emails_standing_invites_when_the_user_store_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(dir.path(), true, true).with_bootstrap_admin("founder@acme.test");
    h.deps.users = Arc::new(FailingUserStore);

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&[]),
        &graph("owner", None),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(
        reports[0].status,
        DeliveryStatus::Sent,
        "an unreadable store must still mail the standing invite: {reports:?}"
    );
    assert_eq!(reports[0].reason, DeliveryReason::OwnerEmailed);
    assert_eq!(h.mail.sent().len(), 1);
    assert_eq!(h.mail.sent()[0].1.to, "founder@acme.test");
}

// --- email ---------------------------------------------------------------

/// The happy path: granted AND established. The mail goes out and is
/// mirrored into the company inbox as outbound, for audit.
#[tokio::test]
async fn email_granted_and_established_sends_and_records_outbound() {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::new(dir.path(), true, true);
    h.receive_from("ada@example.com").await;

    let reports = deliver_outputs(
        Some(&h.deps),
        &record(&["email.send"]),
        &graph("email", Some("ada@example.com")),
        "run-1",
        &reached_output(),
        &[],
    )
    .await;

    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].status, DeliveryStatus::Sent);
    assert_eq!(h.mail.sent().len(), 1);
    assert_eq!(h.mail.sent()[0].1.to, "ada@example.com");

    let messages = h.inbox_messages().await;
    let outbound: Vec<&EmailRecord> = messages.iter().filter(|m| m.outbound).collect();
    assert_eq!(outbound.len(), 1, "the send must leave an audit record");
    assert!(outbound[0].body.contains("Q3 is up 12%."));
}
