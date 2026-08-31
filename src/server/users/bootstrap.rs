//! Issuing the *first* password for a company, from the host.
//!
//! # Why this exists
//!
//! Every way into a new company runs through a credential the deployment may
//! not be able to deliver (#1718):
//!
//! - `POST …/auth/password` needs a session, which is what we are trying to get.
//! - `POST …/users/{id}/password` and the invite routes need an existing
//!   **admin**, and on a first boot there is none.
//! - The magic link needs a mail transport. Its code is minted and stored
//!   *hashed*, so on a host with no transport the credential exists and is
//!   unreachable.
//! - The dev echo of that code is gated on [`AppConfig::is_local_only`], which
//!   is false for exactly the hosted deployment that has this problem.
//! - The platform hub needs the hub wired.
//!
//! So a self-hosted company with no mail and no hub could not be signed into at
//! all. The console said as much — *"an admin can issue you one if you have
//! none"* — with nobody to ask.
//!
//! # Why the host, and not another route
//!
//! This is deliberately **not** reachable over HTTP. The authority it relies on
//! is possession of the process and its storage, which an operator already has
//! and a request never does. Adding an HTTP surface would mean inventing a way
//! to authenticate the one caller who cannot yet authenticate.
//!
//! # What it will not do
//!
//! It issues a password only to an address that is *already* eligible — named
//! in the manifest's `[users] admins`, or injected as the deployment's
//! bootstrap admin. It cannot invent membership, so it is not a way to add
//! someone to a company; it only makes an existing standing invite usable
//! without mail.

use std::sync::Arc;

use crate::error::OpenCompanyError;
use crate::ports::generate_id;
use crate::ports::types::CompanyId;
use crate::ports::users::{UserRecord, UserRole, UserStatus, UserStore, normalize_email};
use crate::server::users::{password, token};

/// What [`issue_password`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    /// The address the password now belongs to, normalized.
    pub email: String,
    /// Whether the account was created, as opposed to an existing one updated.
    pub created: bool,
    /// Whether the holder must replace this password before doing anything else.
    pub must_change_password: bool,
}

/// The addresses a company admits without an invite record: its manifest
/// admins, plus the deployment's bootstrap admin when one is injected.
///
/// Both are the same grant, so they are one list. Shared with the HTTP path's
/// `bootstrap_admins` rather than re-derived, so the CLI cannot come to a
/// different answer than the login route about who is eligible.
pub fn standing_admins(manifest_admins: &[String], bootstrap_admin: Option<&str>) -> Vec<String> {
    let mut admins: Vec<String> = manifest_admins
        .iter()
        .map(|a| normalize_email(a))
        .filter(|email| !email.is_empty())
        .fold(Vec::new(), |mut admins, email| {
            if !admins.contains(&email) {
                admins.push(email);
            }
            admins
        });
    if let Some(email) = bootstrap_admin
        .map(normalize_email)
        .filter(|e| !e.is_empty())
        && !admins.contains(&email)
    {
        admins.push(email);
    }
    admins
}

/// The stores and company context used to issue a password.
///
/// Keeping these related inputs together also makes the host-side operation
/// harder to call with the wrong stores or company.
pub struct PasswordIssueContext<'a> {
    /// User persistence.
    pub users: &'a Arc<dyn UserStore>,
    /// Session persistence.
    pub sessions: &'a Arc<dyn crate::ports::sessions::SessionStore>,
    /// Login-code persistence.
    pub login_codes: &'a Arc<dyn crate::ports::login_codes::LoginCodeStore>,
    /// Company receiving the password.
    pub company: &'a CompanyId,
    /// Admin addresses declared by the company manifest.
    pub manifest_admins: &'a [String],
    /// Optional deployment-provided standing admin.
    pub bootstrap_admin: Option<&'a str>,
}

/// Sets `email`'s password in `company`, creating the account if the address is
/// eligible and has none.
///
/// `require_change` flags the account so the holder must replace the password
/// before doing anything else — the same treatment an admin-issued temporary
/// password gets, and the right default when the operator and the eventual
/// holder are different people.
pub async fn issue_password(
    context: PasswordIssueContext<'_>,
    email: &str,
    plaintext: &str,
    require_change: bool,
) -> Result<Issued, OpenCompanyError> {
    let PasswordIssueContext {
        users,
        sessions,
        login_codes,
        company,
        manifest_admins,
        bootstrap_admin,
    } = context;
    let email = normalize_email(email);
    if email.is_empty() {
        return Err(OpenCompanyError::InvalidRequest(
            "an email address is required".into(),
        ));
    }

    // Validated before anything is written, and against the address it will
    // belong to — `validate` refuses a password that contains its own email.
    password::validate(plaintext, &email)?;

    let existing = users.find_user_by_email(company, &email).await?;

    // Existing accounts may only be reset while retaining their administrative
    // role. A removed standing grant does not erase historical admin status.
    if let Some(existing) = existing.as_ref()
        && existing.role != UserRole::Admin
    {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "{email} is not an admin account and cannot receive a host password reset"
        )));
    }

    // A suspended account cannot sign in at all — the password login path
    // refuses every non-active user — so committing a new password here would
    // only claim a success that can never be used. Refuse it outright instead
    // of persisting an unusable credential.
    if let Some(existing) = existing.as_ref()
        && existing.status != UserStatus::Active
    {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "{email} is suspended and cannot receive a host password reset"
        )));
    }

    // Eligibility is only consulted when there is no account yet. An address
    // that already holds one keeps it even if the manifest later stops naming
    // them: removing someone is `status`, not a silent inability to reset.
    if existing.is_none() {
        let admins = standing_admins(manifest_admins, bootstrap_admin);
        if !admins.contains(&email) {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "{email} is not a standing admin of `{}`, so there is no account to issue a \
                 password for. Add the address to the manifest's [users] admins, or set the \
                 deployment's bootstrap admin, and try again. This command makes an existing \
                 grant usable without mail; it does not create one.",
                company.as_ref()
            )));
        }
    }

    let hash = password::hash(&token::OsTokens, plaintext)?;
    let now = crate::ports::now_millis();
    let created = existing.is_none();

    let user = match existing {
        Some(mut user) => {
            user.password_hash = Some(hash);
            user.must_change_password = require_change;
            user.updated_at_millis = now;
            user
        }
        None => UserRecord {
            // The same id scheme the login path mints, so an account created
            // here is indistinguishable from one created by a magic link.
            id: generate_id(),
            email: email.clone(),
            display_name: None,
            avatar: None,
            // Eligibility above proved this address is a standing *admin*;
            // there is no other role this path can mint.
            role: UserRole::Admin,
            status: UserStatus::Active,
            password_hash: Some(hash),
            must_change_password: require_change,
            created_at_millis: now,
            last_seen_at_millis: None,
            updated_at_millis: now,
        },
    };

    // Revoke old credentials before the password commit. If either revocation
    // fails, no new password is persisted and the old credential state remains
    // the only usable state.
    if !created {
        sessions.delete_for_user(company, &user.id).await?;
    }
    login_codes.delete_for_email(company, &user.email).await?;
    users.upsert_user(company, &user).await?;
    // A newly minted account is the same materialization the login path
    // produces on redemption, so mark any outstanding invite redeemed the same
    // way — a manifest admin's bootstrapped invite record otherwise reads as
    // still pending beside an account that now exists. A bootstrap admin with
    // no invite record is a no-op.
    if created && let Some(mut invite) = users.find_invite_by_email(company, &email).await? {
        invite.accepted_at_millis = Some(now);
        users.upsert_invite(company, &invite).await?;
    }
    Ok(Issued {
        email,
        created,
        must_change_password: require_change,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::users::InviteRecord;
    use crate::store::FsOps;
    use async_trait::async_trait;

    fn stores(
        dir: &tempfile::TempDir,
    ) -> (
        Arc<dyn UserStore>,
        Arc<dyn crate::ports::sessions::SessionStore>,
        Arc<dyn crate::ports::login_codes::LoginCodeStore>,
    ) {
        let ops = Arc::new(FsOps::new(dir.path().to_path_buf()));
        (ops.clone(), ops.clone(), ops)
    }

    const GOOD: &str = "a long enough bootstrap password";

    fn context<'a>(
        users: &'a Arc<dyn UserStore>,
        sessions: &'a Arc<dyn crate::ports::sessions::SessionStore>,
        login_codes: &'a Arc<dyn crate::ports::login_codes::LoginCodeStore>,
        company: &'a CompanyId,
        manifest_admins: &'a [String],
        bootstrap_admin: Option<&'a str>,
    ) -> PasswordIssueContext<'a> {
        PasswordIssueContext {
            users,
            sessions,
            login_codes,
            company,
            manifest_admins,
            bootstrap_admin,
        }
    }

    #[tokio::test]
    async fn issues_a_first_password_to_the_deployment_bootstrap_admin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        let issued = issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &[],
                Some("Founder@Acme.test"),
            ),
            "founder@acme.test",
            GOOD,
            false,
        )
        .await
        .expect("issues");

        assert!(issued.created, "there was no account before this");
        assert_eq!(issued.email, "founder@acme.test");

        let user = users
            .find_user_by_email(&company, "founder@acme.test")
            .await
            .expect("read")
            .expect("the account now exists");
        assert_eq!(user.role, UserRole::Admin);
        assert_eq!(user.status, UserStatus::Active);
        // The stored value must be a hash, never the password.
        let hash = user.password_hash.expect("a hash was stored");
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(!hash.contains(GOOD));
        assert!(password::verify(GOOD, &hash), "the hash verifies");
    }

    /// The manifest is the other source of a standing grant, and it must work
    /// with no deployment bootstrap admin injected at all.
    #[tokio::test]
    async fn a_manifest_admin_is_eligible_without_a_bootstrap_admin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &["ada@acme.test".into()],
                None,
            ),
            "ada@acme.test",
            GOOD,
            false,
        )
        .await
        .expect("issues");

        assert!(
            users
                .find_user_by_email(&company, "ada@acme.test")
                .await
                .expect("read")
                .is_some()
        );
    }

    /// A stored invite for the address must read as redeemed once the host
    /// password creates the account — otherwise a manifest admin's invite row
    /// sits beside the new account as "still pending" forever. This is the
    /// same stamp the login path writes on redemption.
    #[tokio::test]
    async fn creating_an_account_marks_an_outstanding_invite_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");
        let now = crate::ports::now_millis();
        users
            .upsert_invite(
                &company,
                &InviteRecord {
                    id: "manifest:ada@acme.test".into(),
                    email: "ada@acme.test".into(),
                    role: UserRole::Admin,
                    invited_by: "manifest".into(),
                    created_at_millis: now,
                    expires_at_millis: now + 60_000,
                    accepted_at_millis: None,
                    notified_at_millis: None,
                },
            )
            .await
            .expect("invite stored");

        issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &["ada@acme.test".into()],
                None,
            ),
            "ada@acme.test",
            GOOD,
            false,
        )
        .await
        .expect("issues");

        let invite = users
            .find_invite_by_email(&company, "ada@acme.test")
            .await
            .expect("read")
            .expect("the invite still exists");
        let accepted = invite
            .accepted_at_millis
            .expect("the host password stamped the invite accepted");
        assert!(
            accepted >= now,
            "stamped at {accepted}, after the invite was written at {now}"
        );
    }

    /// The boundary that keeps this from being a back door. Possession of the
    /// host lets an operator issue a password to someone the company already
    /// admits — not add a member.
    #[tokio::test]
    async fn refuses_an_address_the_company_does_not_already_admit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        let error = issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &["ada@acme.test".into()],
                Some("founder@acme.test"),
            ),
            "stranger@elsewhere.test",
            GOOD,
            false,
        )
        .await
        .expect_err("a stranger is not eligible");
        assert!(
            error.to_string().contains("not a standing admin"),
            "{error}"
        );

        assert!(
            users
                .find_user_by_email(&company, "stranger@elsewhere.test")
                .await
                .expect("read")
                .is_none(),
            "nothing may be written for a refused address"
        );
    }

    /// An existing account keeps its password resettable even once the manifest
    /// stops naming it. Removing someone is a status change; it must not become
    /// a silent inability to recover the account.
    #[tokio::test]
    async fn an_existing_account_can_be_reset_without_a_standing_grant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &[],
                Some("ada@acme.test"),
            ),
            "ada@acme.test",
            GOOD,
            false,
        )
        .await
        .expect("first issue");

        // Now nobody is named: no manifest admins, no bootstrap admin.
        let again = issue_password(
            context(&users, &sessions, &login_codes, &company, &[], None),
            "ada@acme.test",
            "a different long password",
            false,
        )
        .await
        .expect("an existing account is still resettable");
        assert!(!again.created, "the account was updated, not recreated");

        let user = users
            .find_user_by_email(&company, "ada@acme.test")
            .await
            .expect("read")
            .expect("still there");
        let hash = user.password_hash.expect("hash");
        assert!(password::verify("a different long password", &hash));
        assert!(
            !password::verify(GOOD, &hash),
            "the old password must stop working"
        );
    }

    /// The host path is not a way to reset a non-admin account. An existing
    /// member account must not become a password the operator controls, even
    /// though the address appears in the manifest admins.
    #[tokio::test]
    async fn refuses_to_reset_an_existing_non_admin_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        // A member already exists for the address.
        users
            .upsert_user(
                &company,
                &UserRecord {
                    id: generate_id(),
                    email: "worker@acme.test".into(),
                    display_name: None,
                    avatar: None,
                    role: UserRole::Member,
                    status: UserStatus::Active,
                    password_hash: None,
                    must_change_password: false,
                    created_at_millis: 1,
                    last_seen_at_millis: None,
                    updated_at_millis: 1,
                },
            )
            .await
            .expect("upsert");

        let error = issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &["worker@acme.test".into()],
                None,
            ),
            "worker@acme.test",
            GOOD,
            false,
        )
        .await
        .expect_err("a non-admin account cannot receive a host reset");
        assert!(
            error.to_string().contains("not an admin account"),
            "{error}"
        );

        let user = users
            .find_user_by_email(&company, "worker@acme.test")
            .await
            .expect("read")
            .expect("the account remains");
        assert!(
            user.password_hash.is_none(),
            "a refused reset must not persist a password"
        );
    }

    /// A suspended admin account cannot use a fresh password — the login path
    /// refuses every non-active user — so issuing one would persist a
    /// credential that can never sign in. The command must refuse and say so,
    /// not report an "updated" password that is unusable.
    #[tokio::test]
    async fn refuses_to_reset_a_suspended_admin_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        // The address belongs to an admin who has been suspended.
        users
            .upsert_user(
                &company,
                &UserRecord {
                    id: generate_id(),
                    email: "ada@acme.test".into(),
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
            .expect("upsert");

        let error = issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &["ada@acme.test".into()],
                None,
            ),
            "ada@acme.test",
            GOOD,
            false,
        )
        .await
        .expect_err("a suspended account cannot receive a host reset");
        assert!(error.to_string().contains("suspended"), "{error}");

        let user = users
            .find_user_by_email(&company, "ada@acme.test")
            .await
            .expect("read")
            .expect("the account remains");
        assert!(
            user.password_hash.is_none(),
            "a refused reset must not persist a password"
        );
    }

    /// Re-issuing must not mint a second account for the same address, which
    /// would leave two rows racing to answer a login.
    #[tokio::test]
    async fn re_issuing_keeps_one_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        for _ in 0..3 {
            issue_password(
                context(
                    &users,
                    &sessions,
                    &login_codes,
                    &company,
                    &[],
                    Some("ada@acme.test"),
                ),
                "ada@acme.test",
                GOOD,
                false,
            )
            .await
            .expect("issues");
        }
        let all = users.list_users(&company).await.expect("list");
        assert_eq!(all.len(), 1, "one address, one account: {all:?}");
    }

    #[tokio::test]
    async fn a_temporary_password_flags_the_account_for_replacement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        let issued = issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &[],
                Some("ada@acme.test"),
            ),
            "ada@acme.test",
            GOOD,
            true,
        )
        .await
        .expect("issues");
        assert!(issued.must_change_password);
        assert!(
            users
                .find_user_by_email(&company, "ada@acme.test")
                .await
                .expect("read")
                .expect("there")
                .must_change_password
        );
    }

    /// A password too weak to set through the console must not be settable here
    /// either — the host path is a different door, not a lower bar.
    #[tokio::test]
    async fn a_weak_password_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (users, sessions, login_codes) = stores(&dir);
        let company = CompanyId::new("acme");

        issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &[],
                Some("ada@acme.test"),
            ),
            "ada@acme.test",
            "short",
            false,
        )
        .await
        .expect_err("too short");

        assert!(
            users
                .find_user_by_email(&company, "ada@acme.test")
                .await
                .expect("read")
                .is_none(),
            "a refused password must leave no account behind"
        );
    }

    #[test]
    fn standing_admins_dedupes_and_normalizes() {
        // The same address in both sources is one grant, and case/whitespace
        // must not make it look like two.
        let admins = standing_admins(&["Ada@Acme.test".into()], Some("  ada@acme.test  "));
        assert_eq!(admins, vec!["ada@acme.test"]);
    }

    #[test]
    fn an_empty_bootstrap_admin_adds_nobody() {
        assert!(standing_admins(&[], Some("   ")).is_empty());
        assert!(standing_admins(&[], None).is_empty());
    }

    /// A session store whose `delete_for_user` always fails, so the reset flow
    /// can be proven to stop before the password commit when a revocation
    /// cannot complete. Everything else delegates to the real filesystem store.
    struct SessionRevocationFailure(Arc<FsOps>);

    #[async_trait]
    impl crate::ports::sessions::SessionStore for SessionRevocationFailure {
        async fn create(
            &self,
            company: &CompanyId,
            session: &crate::ports::sessions::SessionRecord,
        ) -> crate::Result<()> {
            self.0.create(company, session).await
        }
        async fn find_by_token_hash(
            &self,
            company: &CompanyId,
            token_hash: &str,
        ) -> crate::Result<Option<crate::ports::sessions::SessionRecord>> {
            self.0.find_by_token_hash(company, token_hash).await
        }
        async fn list_for_user(
            &self,
            company: &CompanyId,
            user_id: &str,
        ) -> crate::Result<Vec<crate::ports::sessions::SessionRecord>> {
            self.0.list_for_user(company, user_id).await
        }
        async fn delete(&self, company: &CompanyId, id: &str) -> crate::Result<bool> {
            self.0.delete(company, id).await
        }
        async fn delete_for_user(
            &self,
            _company: &CompanyId,
            _user_id: &str,
        ) -> crate::Result<u64> {
            Err(OpenCompanyError::Config(
                "injected session revocation failure".into(),
            ))
        }
        async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> crate::Result<u64> {
            self.0.purge_expired(company, now_millis).await
        }
    }

    /// A login-code store whose `delete_for_email` always fails; same proof for
    /// the second revocation in the reset flow.
    struct LoginCodeRevocationFailure(Arc<FsOps>);

    #[async_trait]
    impl crate::ports::login_codes::LoginCodeStore for LoginCodeRevocationFailure {
        async fn create(
            &self,
            company: &CompanyId,
            code: &crate::ports::login_codes::LoginCodeRecord,
        ) -> crate::Result<()> {
            self.0.create(company, code).await
        }
        async fn latest_for_email(
            &self,
            company: &CompanyId,
            email: &str,
        ) -> crate::Result<Option<crate::ports::login_codes::LoginCodeRecord>> {
            self.0.latest_for_email(company, email).await
        }
        async fn consume(
            &self,
            company: &CompanyId,
            code_hash: &str,
            now_millis: u64,
        ) -> crate::Result<Option<crate::ports::login_codes::LoginCodeRecord>> {
            self.0.consume(company, code_hash, now_millis).await
        }
        async fn delete_for_email(&self, _company: &CompanyId, _email: &str) -> crate::Result<u64> {
            Err(OpenCompanyError::Config(
                "injected login-code revocation failure".into(),
            ))
        }
        async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> crate::Result<u64> {
            self.0.purge_expired(company, now_millis).await
        }
    }

    /// The reset revokes sessions before committing the new password, so a
    /// revocation that cannot complete must abort the reset and leave the old
    /// password the only working credential.
    #[tokio::test]
    async fn a_session_revocation_failure_prevents_the_password_update() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops = Arc::new(FsOps::new(dir.path().to_path_buf()));
        let users: Arc<dyn UserStore> = ops.clone();
        let sessions: Arc<dyn crate::ports::sessions::SessionStore> =
            Arc::new(SessionRevocationFailure(ops.clone()));
        let login_codes: Arc<dyn crate::ports::login_codes::LoginCodeStore> = ops.clone();
        let company = CompanyId::new("acme");

        issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &[],
                Some("ada@acme.test"),
            ),
            "ada@acme.test",
            GOOD,
            false,
        )
        .await
        .expect("first issue");

        let error = issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &[],
                Some("ada@acme.test"),
            ),
            "ada@acme.test",
            "a replacement long password",
            false,
        )
        .await
        .expect_err("the reset must abort when session revocation fails");
        assert!(
            error
                .to_string()
                .contains("injected session revocation failure"),
            "{error}"
        );

        let user = users
            .find_user_by_email(&company, "ada@acme.test")
            .await
            .expect("read")
            .expect("still there");
        let hash = user.password_hash.expect("hash");
        assert!(
            password::verify(GOOD, &hash),
            "the old password still works"
        );
        assert!(
            !password::verify("a replacement long password", &hash),
            "the new password must not have been committed"
        );
    }

    /// Same proof for the login-code revocation, which runs after the session
    /// one and also precedes the password commit.
    #[tokio::test]
    async fn a_login_code_revocation_failure_prevents_the_password_update() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ops = Arc::new(FsOps::new(dir.path().to_path_buf()));
        let users: Arc<dyn UserStore> = ops.clone();
        let sessions: Arc<dyn crate::ports::sessions::SessionStore> = ops.clone();
        let login_codes: Arc<dyn crate::ports::login_codes::LoginCodeStore> = ops.clone();
        let company = CompanyId::new("acme");

        // The login-code revocation runs on creation too (a pending magic link
        // for the address must not outlive the issued password), so create the
        // account with the real store first.
        issue_password(
            context(
                &users,
                &sessions,
                &login_codes,
                &company,
                &[],
                Some("ada@acme.test"),
            ),
            "ada@acme.test",
            GOOD,
            false,
        )
        .await
        .expect("first issue");

        // A pending magic link exists for the address, and revoking it fails:
        // the reset must abort before committing the new password.
        let failing: Arc<dyn crate::ports::login_codes::LoginCodeStore> =
            Arc::new(LoginCodeRevocationFailure(ops.clone()));
        let error = issue_password(
            context(
                &users,
                &sessions,
                &failing,
                &company,
                &[],
                Some("ada@acme.test"),
            ),
            "ada@acme.test",
            "a replacement long password",
            false,
        )
        .await
        .expect_err("the reset must abort when login-code revocation fails");
        assert!(
            error
                .to_string()
                .contains("injected login-code revocation failure"),
            "{error}"
        );

        let user = users
            .find_user_by_email(&company, "ada@acme.test")
            .await
            .expect("read")
            .expect("still there");
        let hash = user.password_hash.expect("hash");
        assert!(
            password::verify(GOOD, &hash),
            "the old password still works"
        );
        assert!(
            !password::verify("a replacement long password", &hash),
            "the new password must not have been committed"
        );
    }
}
