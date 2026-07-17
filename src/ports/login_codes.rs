//! The [`LoginCodeStore`] port: pending single-use magic-link codes.
//!
//! Requesting a login mints a high-entropy code, mails a link containing it, and
//! records only its hash here. Redeeming it exchanges the code for a session.
//! There are no passwords anywhere in the system; this is the only way a human
//! authenticates.
//!
//! Two properties this port is responsible for, both enforced in
//! [`LoginCodeStore::consume`] rather than left to callers:
//!
//! - **Single use.** A code redeems exactly once. Consumption must be atomic, so
//!   that two requests racing on the same code cannot both mint a session. A
//!   read-then-write in the handler would be a check-time/use-time gap.
//! - **Expiry.** Codes are short-lived, so a link sitting in a mailbox (or a
//!   forwarded mail, or a browser history entry) stops being a credential
//!   quickly.
//!
//! The code is high-entropy by construction (see the minting helper), which is
//! what makes brute force infeasible and means this port needs no attempt
//! counter. A short numeric code would need one; that trade was taken
//! deliberately in favor of entropy.
//!
//! Like [`SessionStore`](crate::ports::SessionStore), records here are credential
//! material and must never join the export path.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// One outstanding login code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCodeRecord {
    /// Stable id for the code within the company.
    pub id: String,
    /// Lowercase hex SHA-256 of the login code.
    ///
    /// The plaintext code exists only in the email that was sent. Never store,
    /// log, or return it.
    pub code_hash: String,
    /// The normalized email the code was mailed to. The redeeming session is
    /// bound to the user with this address, so it must not be taken from the
    /// redemption request.
    pub email: String,
    /// Epoch-millis timestamp of when the code was minted.
    pub created_at_millis: u64,
    /// Epoch-millis timestamp after which the code is refused.
    pub expires_at_millis: u64,
    /// Epoch-millis timestamp of redemption; `None` while still pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at_millis: Option<u64>,
}

impl LoginCodeRecord {
    /// Whether the code can still be redeemed at `now_millis`.
    pub fn is_redeemable(&self, now_millis: u64) -> bool {
        self.consumed_at_millis.is_none() && now_millis < self.expires_at_millis
    }
}

/// The company's durable table of pending login codes. Company A's codes MUST be
/// invisible to company B — a code mailed for company A must not authenticate
/// against company B.
#[async_trait]
pub trait LoginCodeStore: Send + Sync {
    /// Inserts a freshly minted code.
    async fn create(&self, company: &CompanyId, code: &LoginCodeRecord) -> Result<()>;

    /// The most recently minted code for an address, spent or not.
    ///
    /// Exists for the resend throttle: the login route needs to know *when* it
    /// last mailed this address, and it cannot ask by hash because it does not
    /// have the code. Returning the record — not just a timestamp — keeps the
    /// port from growing a second, narrower question later.
    ///
    /// Note this is a lookup by address rather than by secret, so unlike
    /// [`consume`](Self::consume) it must never be used to authenticate: it
    /// would let anyone holding an address act on its code.
    async fn latest_for_email(
        &self,
        company: &CompanyId,
        email: &str,
    ) -> Result<Option<LoginCodeRecord>>;

    /// Atomically redeems a code by its hash.
    ///
    /// Returns the record if — and only if — this call is the one that consumed
    /// it: the code existed, was unconsumed, and had not expired at
    /// `now_millis`. Every subsequent call for the same hash returns `None`, as
    /// does an unknown, expired, or already-consumed hash.
    ///
    /// Implementations MUST make the check-and-mark a single atomic step. This
    /// is the only place single-use is enforced.
    async fn consume(
        &self,
        company: &CompanyId,
        code_hash: &str,
        now_millis: u64,
    ) -> Result<Option<LoginCodeRecord>>;

    /// Drops every pending code for an address; returns how many.
    ///
    /// Called when issuing a new code so an address has at most one live code,
    /// and on suspend/removal so a mail already in flight cannot be redeemed.
    async fn delete_for_email(&self, company: &CompanyId, email: &str) -> Result<u64>;

    /// Drops codes that expired at or before `now_millis`; returns how many.
    async fn purge_expired(&self, company: &CompanyId, now_millis: u64) -> Result<u64>;
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn code_is_redeemable_only_while_pending_and_unexpired() {
        let mut code = LoginCodeRecord {
            id: "c1".to_string(),
            code_hash: "abc".to_string(),
            email: "ada@example.com".to_string(),
            created_at_millis: 0,
            expires_at_millis: 100,
            consumed_at_millis: None,
        };
        assert!(code.is_redeemable(99));
        assert!(!code.is_redeemable(100), "expiry is exclusive");

        code.consumed_at_millis = Some(50);
        assert!(!code.is_redeemable(60), "a redeemed code is single-use");
    }

    #[test]
    fn login_code_record_round_trips_as_camel_case() {
        let code = LoginCodeRecord {
            id: "c1".to_string(),
            code_hash: "abc".to_string(),
            email: "ada@example.com".to_string(),
            created_at_millis: 0,
            expires_at_millis: 100,
            consumed_at_millis: None,
        };
        let json = serde_json::to_value(&code).unwrap();
        assert_eq!(json["codeHash"], "abc");
        assert!(json.get("consumedAtMillis").is_none());
        assert_eq!(
            serde_json::from_value::<LoginCodeRecord>(json).unwrap(),
            code
        );
    }
}
