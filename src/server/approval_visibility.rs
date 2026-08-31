//! Who may read an approval's *contents* (issue #618).
//!
//! Membership decides whether you may know an approval **exists**. It does not
//! decide whether you may read what it is about. `payload` carries
//! recipient-bearing tool arguments and `amount_usd` carries money; the rest of
//! [`ApprovalSummary`] — the id, the kind, who asked, which task, when — is what
//! makes stalled work visible, and every member needs it.
//!
//! This is the product's **first per-resource, per-role field restriction**.
//! Everywhere else, role decides whether you may *act* (see
//! [`AdminScopedCompany`](crate::server::ops::scope::AdminScopedCompany)); here
//! it decides which fields of a read you receive. That is a new shape and worth
//! knowing before it is copied.
//!
//! ## Why the split lands here and not on the whole route
//!
//! Refusing the route to non-admins was the obvious alternative and is worse: a
//! Member would lose sight of *why* their work is stalled. Issue #468
//! deliberately keeps a "waiting on approval" indicator on the task card, and
//! that indicator has to survive for the people doing the work, not only for
//! the people who can sign it off.
//!
//! ## Hidden is not absent
//!
//! Redaction does **not** simply blank the fields. `payload: None` already
//! means "this effect carries no arguments", so blanking would make a withheld
//! payment indistinguishable from a no-argument tool call — the console would
//! render an approval that looks empty rather than one it may not show. So a
//! redacted summary sets [`ApprovalSummary::contents_hidden`], and the console
//! says "hidden by your role" instead of showing nothing.

use crate::runtime::types::ApprovalSummary;
use crate::server::graphql::auth::GqlAuth;

/// May this principal read an approval's payload and amount?
///
/// Both arms are written out. `GqlAuth` has exactly two, and a wildcard here
/// would silently grant contents to any arm added later — which is how a role
/// guard decays into a deny-list.
pub(crate) fn may_read_approval_contents(auth: &GqlAuth) -> bool {
    match auth {
        // The role already rides on the principal the route resolved, so
        // nothing has to be threaded down into the domain layer to ask this.
        GqlAuth::User(user) => user.may_administer(),
        // **Fail closed, deliberately.** A platform bearer is the hosting
        // control plane — it provisions and suspends containers and is not a
        // person in the company. It has no need for a tenant's message bodies
        // or payment amounts, and "the machine credential sees everything" is
        // the assumption worth not making. This costs nothing today: the
        // console authenticates as a user, and a prosumer deployment has no
        // platform credential at all (`resolve_claims` requires
        // `platform_auth` to be configured), so no human loses anything.
        //
        // If the hosting layer ever genuinely needs contents, that is a
        // deliberate scope to add here, not a default to inherit.
        GqlAuth::Platform(_) => false,
    }
}

/// May this principal read a run's **deep** trace — the unredacted reasoning,
/// tool arguments and raw output the Observatory shows behind a fold?
///
/// The same rule as [`may_read_approval_contents`]: those bodies can carry
/// credentials and file contents, exactly as an approval payload can, so the
/// admin/tenant boundary applies to them too. Kept beside the approval rule so
/// the two sensitive-content gates cannot drift.
pub(crate) fn may_read_deep_trace(auth: &GqlAuth) -> bool {
    may_read_approval_contents(auth)
}

/// Applies [`may_read_approval_contents`] to a projection on its way out.
///
/// Takes the whole list rather than one summary because every caller has a
/// list, and because doing it per-item at three call sites is three chances to
/// forget one.
pub(crate) fn for_principal(
    auth: &GqlAuth,
    mut approvals: Vec<ApprovalSummary>,
) -> Vec<ApprovalSummary> {
    if may_read_approval_contents(auth) {
        return approvals;
    }
    for approval in &mut approvals {
        hide_contents(approval);
    }
    approvals
}

/// Strips the two contents fields and records that it happened.
///
/// `contents_hidden` is what keeps this honest — see the module note.
pub(crate) fn hide_contents(approval: &mut ApprovalSummary) {
    approval.payload = None;
    approval.amount_usd = None;
    approval.contents_hidden = true;
}

/// The same rule applied to the **executed** effects on a task's detail read
/// (issue #705).
///
/// #618 restricted the money on an approval — the effect a person has *not yet*
/// signed off. The identical amount on the effect once it has run was projected
/// by a different DTO on a different route, and that route was never covered:
/// any Member could `GET` a card and read the dollar value of every irreversible
/// effect on it.
///
/// It lives here, beside [`for_principal`], rather than in the task module,
/// because the thing that must not fork is the *rule*. A second redactor in the
/// tasks route could drift from this one, and two role checks disagreeing about
/// what an operator may see is worse than the leak either was written to close.
///
/// Takes `may_read_contents` rather than the principal because
/// [`ScopedCompany`](crate::server::ops::ScopedCompany) deliberately drops the
/// role at the edge, carrying this decision forward instead — see the field's
/// own note. The decision is still made by
/// [`may_read_approval_contents`] and nowhere else.
///
/// Takes the whole list, like [`for_principal`], so a caller cannot apply it to
/// some entries and forget others.
pub(crate) fn effects_for_principal(
    may_read_contents: bool,
    mut effects: Vec<crate::server::ops::tasks::IrreversibleEffect>,
) -> Vec<crate::server::ops::tasks::IrreversibleEffect> {
    if may_read_contents {
        return effects;
    }
    for effect in &mut effects {
        // `amount_hidden` only where something was actually withheld: an effect
        // that never carried money must not report itself as redacted, or
        // "nothing to show" and "not shown to you" stop being distinguishable
        // in the direction that matters.
        if effect.amount_usd.is_some() {
            effect.amount_usd = None;
            effect.amount_hidden = true;
        }
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::types::{ApprovalId, CompanyId};
    use crate::ports::{SessionKind, UserRole};
    use crate::server::graphql::auth::UserPrincipal;
    use crate::server::platform_auth::PlatformClaims;

    fn principal(role: UserRole) -> GqlAuth {
        GqlAuth::User(UserPrincipal {
            company: CompanyId::new("acme"),
            user_id: "u-1".to_string(),
            email: "who@example.test".to_string(),
            role,
            must_change_password: false,
            session_token_hash: "hash".to_string(),
            credential: SessionKind::Browser,
        })
    }

    /// A summary with both contents fields populated — the only interesting
    /// input, since redaction is a no-op on an approval that carries neither.
    fn summary() -> ApprovalSummary {
        ApprovalSummary {
            id: ApprovalId::new("appr-1"),
            kind: "email.send".to_string(),
            group: crate::ports::types::EffectGroup::Send,
            amount_usd: Some(2400.0),
            at_millis: 1_000,
            expires_at_millis: Some(87_400_000),
            task: None,
            agent: Some("ops".to_string()),
            payload: Some(serde_json::json!({ "to": "board@example.test" })),
            thread: None,
            workflow_run_id: None,
            workflow_id: None,
            broadly_grantable: false,
            broadly_deniable: false,
            contents_hidden: false,
            batch: Some("turn-1".to_string()),
        }
    }

    #[test]
    fn an_admin_reads_the_contents_unchanged() {
        let out = for_principal(&principal(UserRole::Admin), vec![summary()]);
        assert!(out[0].payload.is_some(), "an admin decides the sign-off");
        assert_eq!(out[0].amount_usd, Some(2400.0));
        assert!(!out[0].contents_hidden);
    }

    /// The case the issue exists for. Membership still gets the row — that is
    /// #468's stalled-work signal — but not what is inside it.
    #[test]
    fn a_member_gets_the_approval_without_its_contents() {
        let out = for_principal(&principal(UserRole::Member), vec![summary()]);
        assert!(
            out[0].payload.is_none(),
            "the recipient must not reach a member"
        );
        assert!(out[0].amount_usd.is_none(), "nor the amount");
        assert!(
            out[0].contents_hidden,
            "and the console must be able to say so, rather than render an empty card"
        );
        // Everything that makes stalled work legible survives.
        assert_eq!(out[0].kind, "email.send");
        assert_eq!(out[0].agent.as_deref(), Some("ops"));
        assert_eq!(out[0].at_millis, 1_000);
        // Issue #842: including which requests arrived together. Which turn
        // asked is not *contents* — withholding it would split one batch into
        // unrelated single cards for a member, so the two roles would see the
        // conversation interrupted a different number of times for the same
        // turn. Less detail than an admin gets; the same shape of request.
        assert_eq!(
            out[0].batch.as_deref(),
            Some("turn-1"),
            "role redaction withholds contents, not the grouping"
        );
        // **T11 (issue #971).** The deadline is not contents either, and it is
        // the one field whose absence would actively mislead: a member watching
        // their own stalled work would see a card silently vanish with no
        // warning it was going to, which is the failure shortening the deadline
        // would otherwise introduce. Money and recipients stay withheld — the
        // two assertions above — so this widens nothing.
        assert_eq!(
            out[0].expires_at_millis,
            Some(87_400_000),
            "a member must be told when their stalled work will be given up on"
        );
    }

    /// Issue #618's stated trap: a platform bearer carries no `UserRole`, and
    /// whatever it gets must be a decision rather than a fallthrough. It is
    /// **fail-closed** — see the comment on `may_read_approval_contents`.
    #[test]
    fn a_platform_bearer_is_refused_the_contents_explicitly() {
        let claims = PlatformClaims {
            tenant: "tenant:hosting".to_string(),
            scopes: std::collections::HashSet::new(),
            companies: None,
        };
        let out = for_principal(&GqlAuth::Platform(claims), vec![summary()]);
        assert!(out[0].payload.is_none());
        assert!(out[0].amount_usd.is_none());
        assert!(out[0].contents_hidden);
    }

    /// Redaction must not invent contents where there were none: a no-argument
    /// approval read by an admin still reports `contents_hidden == false`, so
    /// "nothing to show" and "not shown to you" stay distinguishable.
    #[test]
    fn an_absent_payload_is_not_reported_as_hidden() {
        let mut bare = summary();
        bare.payload = None;
        bare.amount_usd = None;
        let out = for_principal(&principal(UserRole::Admin), vec![bare]);
        assert!(!out[0].contents_hidden);
    }

    /// Issue #1418: the workflow origin's *second half* survives redaction.
    ///
    /// `workflow_run_id` already rides through `hide_contents` untouched — it is
    /// structural, not contents. The workflow id must too, or a member holding
    /// up a stalled native `workflow.approve` would keep the run id and lose the
    /// one thing that turns it into an address: exactly the stalled-work
    /// visibility issue #468 exists to protect.
    #[test]
    fn a_member_keeps_the_workflow_origin_when_contents_are_hidden() {
        let mut gate = summary();
        gate.kind = "workflow.approve".to_string();
        gate.payload =
            Some(serde_json::json!({ "workflow_id": "feature_pipeline", "node_id": "spec" }));
        gate.workflow_id = Some("feature_pipeline".to_string());
        let out = for_principal(&principal(UserRole::Member), vec![gate]);
        assert!(out[0].payload.is_none(), "contents stay withheld");
        assert!(
            out[0].contents_hidden,
            "and the card still says it may not show them"
        );
        assert_eq!(
            out[0].workflow_id.as_deref(),
            Some("feature_pipeline"),
            "the workflow origin is an address, not contents, and survives"
        );
    }
}
