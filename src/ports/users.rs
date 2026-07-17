//! The [`UserStore`] port: the company's directory of human collaborators.
//!
//! Users are the people who work alongside the company's agents — they read the
//! console and talk to desks in chat. They are not billing subjects: the
//! platform's Node backend owns accounts and money, and nothing here knows about
//! either. A user exists only inside one company, which is why every method is
//! keyed by [`CompanyId`].
//!
//! Access is invite-only. An [`InviteRecord`] is an admin's standing permission
//! for one email address to become a [`UserRecord`]; the address cannot log in
//! before that invite exists, and redeeming it is what mints the user. Both live
//! behind one port because they share the email keyspace — "invited" and
//! "joined" are two states of the same address and must stay consistent.
//!
//! Credential material is deliberately *not* here: session tokens live in
//! [`SessionStore`](crate::ports::SessionStore) and login codes in
//! [`LoginCodeStore`](crate::ports::LoginCodeStore), so they can carry their own
//! expiry/purge rules and stay out of any export path.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// What a user is allowed to do inside their company.
///
/// Deliberately two-valued: the product need is "who may invite others", not a
/// permission matrix. Anything finer belongs in a later, explicit design.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    /// May invite and remove other users, in addition to everything a member can do.
    Admin,
    /// May use the company — read the console, chat with desks.
    #[default]
    Member,
}

impl UserRole {
    /// Whether this role may invite, revoke, and remove other users.
    pub fn may_administer(&self) -> bool {
        matches!(self, UserRole::Admin)
    }
}

/// Whether a user may currently authenticate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserStatus {
    /// Normal, may log in.
    #[default]
    Active,
    /// Retained for attribution, but refused at login and on every request.
    Suspended,
}

/// One human collaborator in a company.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRecord {
    /// Stable id for the user within the company. Used as [`Actor::id`](crate::ports::types::Actor)
    /// when attributing this user's chat messages, so it must outlive the email.
    pub id: String,
    /// The user's email address, already normalized by [`normalize_email`].
    /// Unique within the company.
    pub email: String,
    /// An optional human-readable name for the console to render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What the user may do.
    pub role: UserRole,
    /// Whether the user may currently authenticate.
    pub status: UserStatus,
    /// Argon2id PHC hash of the user's password, if they have set one.
    ///
    /// `None` means magic-link only — the common case, and the state every user
    /// starts in. Never the password itself: see
    /// [`password`](crate::server::users::password). This field must never be
    /// serialized into a route response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    /// Whether the user must replace their password before doing anything
    /// else, set when an admin issues a temporary one.
    ///
    /// A real boundary, not a hint: the auth extractors refuse a flagged
    /// session with `403 password_change_required` everywhere except
    /// set-password, logout, and `me`. An admin who resets a password knows it
    /// and conveys it over a channel they do not control, so a session opened
    /// with one is good for exactly one thing.
    #[serde(default)]
    pub must_change_password: bool,
    /// Epoch-millis timestamp of when the user redeemed their invite.
    pub created_at_millis: u64,
    /// Epoch-millis timestamp of the user's most recent **sign-in**.
    ///
    /// Stamped when a session is minted — by link or by password — not on every
    /// authenticated request. Tracking activity would cost a store write per
    /// call, which is not worth knowing someone was here a minute ago.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at_millis: Option<u64>,
    /// Epoch-millis timestamp of the last update to this record.
    pub updated_at_millis: u64,
}

/// An admin's standing permission for one email address to join the company.
///
/// An invite is not a credential — it grants no access on its own and is safe to
/// list back to admins. It only makes the address *eligible* to request a login
/// code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRecord {
    /// Stable id for the invite within the company.
    pub id: String,
    /// The invited email address, already normalized by [`normalize_email`].
    /// Unique within the company.
    pub email: String,
    /// The role the user will be created with when they redeem this invite.
    pub role: UserRole,
    /// Who sent the invite, as an [`Actor`](crate::ports::types::Actor) id. The
    /// operator token invites as `"operator"`.
    pub invited_by: String,
    /// Epoch-millis timestamp of when the invite was created.
    pub created_at_millis: u64,
    /// Epoch-millis timestamp after which the invite is no longer redeemable.
    pub expires_at_millis: u64,
    /// Epoch-millis timestamp of redemption; `None` while still outstanding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at_millis: Option<u64>,
}

impl InviteRecord {
    /// Whether this invite can still be redeemed at `now_millis`.
    pub fn is_redeemable(&self, now_millis: u64) -> bool {
        self.accepted_at_millis.is_none() && now_millis < self.expires_at_millis
    }
}

/// Normalizes an email address into its canonical storage/lookup form.
///
/// Trims surrounding whitespace and lowercases. The local part of an address is
/// technically case-sensitive per RFC 5321, but no mail provider in practice
/// treats it so, and matching case-sensitively here would let one person hold
/// two accounts (and two invites) for what is really one mailbox. Every write
/// and every lookup must go through this, or the uniqueness index is a lie.
pub fn normalize_email(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// The company's durable user directory. Company A's users MUST be invisible to
/// company B.
///
/// Implementations must enforce that `email` is unique within a company for
/// users and for invites (independently — an outstanding invite and an existing
/// user may briefly share an address during redemption). Lookups by email are on
/// the login hot path and must be indexed, not scanned.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// Lists every user in the company, most-recently-created first.
    async fn list_users(&self, company: &CompanyId) -> Result<Vec<UserRecord>>;
    /// Fetches one user by id.
    async fn get_user(&self, company: &CompanyId, id: &str) -> Result<Option<UserRecord>>;
    /// Fetches one user by normalized email. The caller must pass the output of
    /// [`normalize_email`].
    async fn find_user_by_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<UserRecord>>;
    /// Inserts or replaces a user by id.
    async fn upsert_user(&self, company: &CompanyId, user: &UserRecord) -> Result<()>;
    /// Deletes a user by id; returns whether one was removed.
    async fn delete_user(&self, company: &CompanyId, id: &str) -> Result<bool>;

    /// Lists every invite in the company, most-recently-created first.
    async fn list_invites(&self, company: &CompanyId) -> Result<Vec<InviteRecord>>;
    /// Fetches one invite by normalized email. The caller must pass the output
    /// of [`normalize_email`].
    async fn find_invite_by_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<InviteRecord>>;
    /// Inserts or replaces an invite by id.
    async fn upsert_invite(&self, company: &CompanyId, invite: &InviteRecord) -> Result<()>;
    /// Deletes an invite by id; returns whether one was removed.
    async fn delete_invite(&self, company: &CompanyId, id: &str) -> Result<bool>;
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn normalize_email_folds_case_and_trims() {
        assert_eq!(normalize_email("  Ada@Example.COM \n"), "ada@example.com");
        assert_eq!(normalize_email("ada@example.com"), "ada@example.com");
    }

    #[test]
    fn only_admins_may_administer() {
        assert!(UserRole::Admin.may_administer());
        assert!(!UserRole::Member.may_administer());
    }

    #[test]
    fn roles_and_statuses_default_to_least_privilege() {
        // A record deserialized without these fields must not become an admin.
        assert_eq!(UserRole::default(), UserRole::Member);
        assert_eq!(UserStatus::default(), UserStatus::Active);
    }

    #[test]
    fn invite_is_redeemable_only_while_outstanding_and_unexpired() {
        let mut invite = InviteRecord {
            id: "i1".to_string(),
            email: "ada@example.com".to_string(),
            role: UserRole::Member,
            invited_by: "operator".to_string(),
            created_at_millis: 0,
            expires_at_millis: 100,
            accepted_at_millis: None,
        };
        assert!(invite.is_redeemable(99));
        // Expiry is exclusive: at the boundary the invite is already dead.
        assert!(!invite.is_redeemable(100));
        assert!(!invite.is_redeemable(101));

        invite.accepted_at_millis = Some(50);
        assert!(!invite.is_redeemable(60), "a redeemed invite is single-use");
    }

    #[test]
    fn user_record_round_trips_as_camel_case() {
        let user = UserRecord {
            id: "u1".to_string(),
            email: "ada@example.com".to_string(),
            display_name: Some("Ada".to_string()),
            role: UserRole::Admin,
            status: UserStatus::Active,
            password_hash: None,
            must_change_password: false,
            created_at_millis: 1,
            last_seen_at_millis: None,
            updated_at_millis: 2,
        };
        let json = serde_json::to_value(&user).unwrap();
        assert_eq!(json["createdAtMillis"], 1);
        assert_eq!(json["role"], "admin");
        assert_eq!(json["status"], "active");
        // Absent optionals stay absent rather than serializing as null.
        assert!(json.get("lastSeenAtMillis").is_none());
        assert!(
            json.get("passwordHash").is_none(),
            "a user with no password must not carry a null hash field"
        );
        assert_eq!(serde_json::from_value::<UserRecord>(json).unwrap(), user);
    }

    #[test]
    fn a_user_stored_before_passwords_existed_still_loads() {
        // Records written by the magic-link-only build carry neither field.
        // They must load as "no password, nothing to change" rather than fail.
        let json = serde_json::json!({
            "id": "u1",
            "email": "ada@example.com",
            "role": "member",
            "status": "active",
            "createdAtMillis": 1,
            "updatedAtMillis": 2,
        });
        let user: UserRecord = serde_json::from_value(json).unwrap();
        assert_eq!(user.password_hash, None);
        assert!(!user.must_change_password);
    }

    #[test]
    fn a_password_hash_round_trips_when_set() {
        let user = UserRecord {
            id: "u1".to_string(),
            email: "ada@example.com".to_string(),
            display_name: None,
            role: UserRole::Member,
            status: UserStatus::Active,
            password_hash: Some("$argon2id$v=19$...".to_string()),
            must_change_password: true,
            created_at_millis: 1,
            last_seen_at_millis: None,
            updated_at_millis: 2,
        };
        let json = serde_json::to_value(&user).unwrap();
        assert_eq!(json["mustChangePassword"], true);
        assert_eq!(serde_json::from_value::<UserRecord>(json).unwrap(), user);
    }
}
