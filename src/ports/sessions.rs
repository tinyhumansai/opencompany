//! The [`SessionStore`] port: durable, revocable proof that a user logged in.
//!
//! A session is minted when a user redeems a login code and is carried by the
//! console in an `HttpOnly` cookie. The plaintext token is handed to the browser
//! exactly once and is never written down: only its hash reaches this port (see
//! [`SessionRecord::token_hash`]). A dump of this store therefore cannot be
//! replayed as anyone.
//!
//! Sessions are per-company, like every other port. That is not merely tidiness:
//! in local development one process serves many companies from one origin, so a
//! session minted for company A must be unusable against company B. Keying every
//! method by [`CompanyId`] is what makes that structural rather than a check
//! someone might forget.
//!
//! Session records are credential material and must stay out of
//! `opencompany export` — the export path covers the company/event/memory/context
//! ports only, and this port must not be added to it.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// One logged-in browser session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    /// Stable id for the session within the company. Safe to show a user when
    /// listing their sessions, and to revoke by — unlike the token.
    pub id: String,
    /// Lowercase hex SHA-256 of the session token.
    ///
    /// The plaintext token exists only in the response that minted it and in the
    /// browser's cookie jar. Never store, log, or return it.
    pub token_hash: String,
    /// The [`UserRecord::id`](crate::ports::UserRecord) this session authenticates.
    pub user_id: String,
    /// Epoch-millis timestamp of when the session was minted.
    pub created_at_millis: u64,
    /// Epoch-millis timestamp after which the session is refused.
    pub expires_at_millis: u64,
    /// Epoch-millis timestamp of the session's most recent authenticated request.
    pub last_seen_at_millis: u64,
    /// The `User-Agent` that minted the session, so a user can recognize a
    /// session when revoking it. Untrusted, display-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

impl SessionRecord {
    /// Whether the session is still valid at `now_millis`.
    pub fn is_live(&self, now_millis: u64) -> bool {
        now_millis < self.expires_at_millis
    }
}

/// The company's durable session table. Company A's sessions MUST be invisible
/// to company B.
///
/// Lookup by token hash is on every authenticated request's hot path and must be
/// indexed, not scanned.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Inserts a new session.
    async fn create(&self, company: &CompanyId, session: &SessionRecord) -> Result<()>;
    /// Fetches a session by its token hash.
    ///
    /// Returns the record even when expired; callers decide, so that an expired
    /// session is distinguishable from an unknown one for purging. Authentication
    /// paths must check [`SessionRecord::is_live`].
    async fn find_by_token_hash(
        &self,
        company: &CompanyId,
        token_hash: &str,
    ) -> Result<Option<SessionRecord>>;
    /// Lists a user's live and expired sessions, most-recently-created first.
    async fn list_for_user(&self, company: &CompanyId, user_id: &str)
    -> Result<Vec<SessionRecord>>;
    /// Records activity on a session, for the console's session list.
    async fn touch(&self, company: &CompanyId, id: &str, at_millis: u64) -> Result<()>;
    /// Revokes one session by id; returns whether one was removed.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool>;
    /// Revokes every session belonging to a user; returns how many were removed.
    ///
    /// This is the lever behind suspending or deleting a user: without it, a
    /// removed user keeps working until their cookie happens to expire.
    async fn delete_for_user(&self, company: &CompanyId, user_id: &str) -> Result<u64>;
    /// Drops sessions that expired at or before `now_millis`; returns how many.
    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64>;
}

#[cfg(test)]
mod test {
    use super::*;

    fn session() -> SessionRecord {
        SessionRecord {
            id: "s1".to_string(),
            token_hash: "abc".to_string(),
            user_id: "u1".to_string(),
            created_at_millis: 0,
            expires_at_millis: 100,
            last_seen_at_millis: 0,
            user_agent: None,
        }
    }

    #[test]
    fn session_is_live_until_its_expiry() {
        let s = session();
        assert!(s.is_live(99));
        // Expiry is exclusive, matching InviteRecord::is_redeemable.
        assert!(!s.is_live(100));
        assert!(!s.is_live(101));
    }

    #[test]
    fn session_record_round_trips_as_camel_case() {
        let s = session();
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["tokenHash"], "abc");
        assert_eq!(json["userId"], "u1");
        assert!(json.get("userAgent").is_none());
        assert_eq!(serde_json::from_value::<SessionRecord>(json).unwrap(), s);
    }
}
