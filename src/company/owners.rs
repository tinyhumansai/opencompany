//! Who "the company's owner" is, resolved **server-side** from the roster.
//!
//! [`owner_recipients`] is shared by the two callers that must reach a company's
//! owners by email — workflow delivery's `owner` destination
//! ([`crate::workflows::delivery`]) and the parked-approval notification
//! ([`crate::runtime::cycle`], issue #750). Both carry the same hard boundary:
//! recipients resolve from the company's own admins and standing invites, and a
//! caller never names an address. Keeping **one** resolver means that boundary
//! is written once and cannot drift between the two paths.
//!
//! It lives here, outside the `openhuman`-gated `workflows` module, precisely so
//! the default-build cycle can reuse it — the resolution is pure roster logic
//! with no harness dependency.

use crate::ports::types::{CompanyId, CompanyRecord};
use crate::ports::{UserRole, UserStatus, UserStore, normalize_email};

/// The addresses an `owner` report is emailed to: the company's active admins,
/// unioned with its **standing admin invites** — the manifest's `[users] admins`
/// and the deployment's bootstrap admin (`bootstrap_admin`, issue #661 / M8) —
/// that have not yet signed in.
///
/// # Why the union, and why the "no user record" restriction
///
/// A platform-provisioned company names nobody in its manifest and has nobody in
/// the [`UserStore`] until the creator redeems their first login link. The pre-M8
/// resolver read only the store, so on a fresh tenant an owner report found no
/// admin address and fell back to the operator channel — the one human who could
/// act on it never got it. The standing invites are exactly the addresses the
/// login path (`server::users::eligibility` / `bootstrap_admins`) already treats
/// as admins-in-waiting, so `owner` mails them for the same reason they can log
/// in.
///
/// A **user record wins** over a standing invite for the same address, mirroring
/// `eligibility`: a standing invite is only mailed when the address holds *no*
/// record at all. Two consequences fall out of that one rule —
///
/// * a bootstrap admin who has since signed in **and been suspended** is not
///   mailed (their record wins, and a suspended admin is not an active one), and
/// * an address named both as an active admin and as a standing invite is mailed
///   **once** (the active-admin arm sends it; the standing copy is dropped as
///   "already has a record").
///
/// # Store-error stance: still mail the standing invites
///
/// An unreadable user store yields the standing invites **anyway**, not an empty
/// list. The store failing is precisely when dropping the only humans the company
/// is known to have is worst — that silent drop back to the operator channel is
/// the M8 bug. The read failure is logged; the standing invites, which come from
/// the manifest and the injected config and need no store read, are still mailed.
/// An empty result (no admins, no standing invites) routes `owner` to the
/// operator-channel fallback exactly as before.
pub(crate) async fn owner_recipients(
    users: &dyn UserStore,
    company: &CompanyId,
    record: &CompanyRecord,
    bootstrap_admin: Option<&str>,
) -> Vec<String> {
    // The standing admin invites: the manifest's `[users] admins` plus the
    // platform-injected bootstrap admin, normalized the same way the login path
    // normalizes them so `Grace@ACME.test` and `grace@acme.test` are one address
    // here and there. `bootstrap_admin` arrives already normalized (the
    // `AppConfig` accessor did it), but normalizing again is idempotent and keeps
    // this function honest against a caller that passes a raw value.
    let mut standing: Vec<String> = record
        .manifest
        .users
        .admins
        .iter()
        .map(|a| normalize_email(a))
        .collect();
    if let Some(email) = bootstrap_admin {
        let email = normalize_email(email);
        if !email.is_empty() && !standing.contains(&email) {
            standing.push(email);
        }
    }

    match users.list_users(company).await {
        Ok(list) => {
            // Every address that holds a record, whatever its role or status.
            // These win: a standing invite for such an address is dropped, so a
            // suspended admin is not resurrected through a leftover invite and a
            // double-listed address is mailed once.
            let has_record: std::collections::HashSet<String> =
                list.iter().map(|u| normalize_email(&u.email)).collect();
            // The send-eligible records: active admins with a real mailbox.
            let mut recipients: Vec<String> = list
                .iter()
                .filter(|u| u.role == UserRole::Admin && u.status == UserStatus::Active)
                .map(|u| u.email.clone())
                .filter(|email| email.contains('@'))
                .collect();
            // Standing invites with no record yet, and a real mailbox.
            for email in standing {
                if !has_record.contains(&email)
                    && email.contains('@')
                    && !recipients.contains(&email)
                {
                    recipients.push(email);
                }
            }
            recipients
        }
        Err(err) => {
            tracing::warn!(
                company = %company,
                error = %err,
                "owner recipients: could not read the user directory; emailing the standing admin \
                 invites only (dropping them would silence the owner report entirely)"
            );
            // Still mail the standing invites — they are the only humans the
            // company is known to have, and this drop is the M8 bug.
            standing
                .into_iter()
                .filter(|email| email.contains('@'))
                .collect()
        }
    }
}
