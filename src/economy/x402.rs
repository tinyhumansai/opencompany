//! x402 payment challenges and Ed25519-signed authorizations.
//!
//! When a counterparty gates a skill behind payment it answers `402` with a
//! challenge naming the `amount`, `recipient`, `asset`, and `network`. The payer
//! signs an **authorization** over a canonical payload with the same Ed25519
//! identity key it uses for SIWX, then posts it to the settlement endpoints.
//! This module only *builds and verifies* authorizations — no on-chain
//! submission happens here (that is a documented SDK gap).
//!
//! ## Canonical byte layout (golden, versioned)
//!
//! ```text
//! tiny.place-x402-v1\n
//! <agentId>\n
//! <amount>\n
//! <recipient>\n
//! <asset>\n
//! <network>\n
//! <nonce>\n
//! <timestamp>
//! ```
//!
//! Isolated in [`canonical_bytes`] so it is a one-function change to reconcile
//! with the real tiny.place server when reachable.
//!
//! ## The nonce comes from the OS CSPRNG
//!
//! `nonce` is documented as single-use and is signed into the payload above,
//! so a counterparty's replay check is only as good as the value's
//! unpredictability and uniqueness. It is therefore minted by [`mint_nonce`]
//! from 256 bits of OS randomness through the same
//! [`TokenSource`](crate::server::users::token::TokenSource) seam the user-auth
//! secrets use — **not** from
//! [`generate_id`](crate::ports::generate_id), whose epoch-millis-plus-counter
//! shape is guessable from a prior value and repeats across processes that
//! start in the same millisecond.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::economy::signer::{LocalSigner, verify_b58};
use crate::error::OpenCompanyError;
use crate::server::platform_auth::b64url_encode;
use crate::server::users::token::{OsTokens, TokenSource};

/// The domain-separation tag pinning the x402 canonical layout version.
pub const X402_DOMAIN: &str = "tiny.place-x402-v1";

/// A payment challenge parsed from a counterparty's `402` response body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct X402Challenge {
    /// The amount due, as a decimal string (e.g. `"25.00"`).
    pub amount: String,
    /// The recipient address to pay.
    pub recipient: String,
    /// The settlement asset (e.g. `"USDC"`).
    pub asset: String,
    /// The settlement network (e.g. `"solana"`).
    pub network: String,
}

impl X402Challenge {
    /// Parses a challenge from a `402` JSON body.
    ///
    /// Accepts either a flat object (`{amount, recipient, asset, network}`) or
    /// the x402 `{ "accepts": [ { … } ] }` envelope, and tolerates the common
    /// field aliases `maxAmountRequired`/`payTo`.
    pub fn from_body(v: &serde_json::Value) -> Result<Self> {
        let obj = v.get("accepts").and_then(|a| a.get(0)).unwrap_or(v);

        let amount = string_field(obj, &["amount", "maxAmountRequired"]).ok_or_else(|| {
            OpenCompanyError::InvalidRequest("x402 challenge is missing `amount`".into())
        })?;
        let recipient = string_field(obj, &["recipient", "payTo"]).ok_or_else(|| {
            OpenCompanyError::InvalidRequest("x402 challenge is missing `recipient`".into())
        })?;
        let asset = string_field(obj, &["asset"]).unwrap_or_else(|| "USDC".to_string());
        let network = string_field(obj, &["network"]).unwrap_or_else(|| "solana".to_string());

        Ok(Self {
            amount,
            recipient,
            asset,
            network,
        })
    }
}

/// A signed x402 payment authorization, ready to POST to `/payments/verify`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct X402Authorization {
    /// The payer's base58 `agentId`.
    #[serde(rename = "agentId")]
    pub agent_id: String,
    /// The amount authorized. May exceed the challenge amount for an `upto`
    /// delegated-signer grant.
    pub amount: String,
    /// The recipient address.
    pub recipient: String,
    /// The settlement asset.
    pub asset: String,
    /// The settlement network.
    pub network: String,
    /// A single-use nonce: 256 bits of OS randomness, base64url, 43 chars.
    ///
    /// Opaque to every reader — nothing parses, stores, or matches its shape —
    /// so the counterparty only needs it to be unpredictable and unique. See
    /// [`mint_nonce`].
    pub nonce: String,
    /// The authorization timestamp, epoch seconds.
    pub timestamp: i64,
    /// The base58 Ed25519 signature over [`canonical_bytes`].
    #[serde(rename = "signature")]
    pub signature_b58: String,
}

/// How many random bytes back an authorization nonce. 32 bytes = 256 bits,
/// matching the user-auth secrets, so two mints colliding is not a scenario.
const NONCE_BYTES: usize = 32;

/// Mints an authorization nonce: 256 bits from `src`, base64url, 43 chars.
///
/// A pure function of the source bytes — no clock, no counter, no process
/// state — which is the property that makes one nonce say nothing about the
/// next, and makes two processes minting in the same millisecond differ.
pub fn mint_nonce(src: &dyn TokenSource) -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    src.fill(&mut bytes);
    b64url_encode(&bytes)
}

/// Builds the canonical bytes an x402 authorization signs. See module docs.
pub fn canonical_bytes(
    agent_id: &str,
    amount: &str,
    recipient: &str,
    asset: &str,
    network: &str,
    nonce: &str,
    timestamp: i64,
) -> Vec<u8> {
    format!(
        "{X402_DOMAIN}\n{agent_id}\n{amount}\n{recipient}\n{asset}\n{network}\n{nonce}\n{timestamp}"
    )
    .into_bytes()
}

/// Signs an authorization paying exactly the challenged amount.
pub fn authorize(signer: &LocalSigner, ch: &X402Challenge, now: i64) -> X402Authorization {
    authorize_amount(signer, ch, ch.amount.clone(), now)
}

/// Signs a delegated-signer `upto` authorization capped at `cap`, letting the
/// counterparty settle any amount up to the cap.
pub fn authorize_upto(
    signer: &LocalSigner,
    ch: &X402Challenge,
    cap: &str,
    now: i64,
) -> X402Authorization {
    authorize_amount(signer, ch, cap.to_string(), now)
}

fn authorize_amount(
    signer: &LocalSigner,
    ch: &X402Challenge,
    amount: String,
    now: i64,
) -> X402Authorization {
    let agent_id = signer.agent_id();
    let nonce = mint_nonce(&OsTokens);
    let msg = canonical_bytes(
        &agent_id,
        &amount,
        &ch.recipient,
        &ch.asset,
        &ch.network,
        &nonce,
        now,
    );
    let signature_b58 = signer.sign_b58(&msg);
    X402Authorization {
        agent_id,
        amount,
        recipient: ch.recipient.clone(),
        asset: ch.asset.clone(),
        network: ch.network.clone(),
        nonce,
        timestamp: now,
        signature_b58,
    }
}

/// Verifies an authorization's signature against its own declared `agentId`.
pub fn verify(auth: &X402Authorization) -> Result<()> {
    let msg = canonical_bytes(
        &auth.agent_id,
        &auth.amount,
        &auth.recipient,
        &auth.asset,
        &auth.network,
        &auth.nonce,
        auth.timestamp,
    );
    verify_b58(&auth.agent_id, &msg, &auth.signature_b58)
}

fn string_field(obj: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = obj.get(*key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use super::*;

    fn sample_challenge() -> X402Challenge {
        X402Challenge {
            amount: "25.00".into(),
            recipient: "RecipientAddr".into(),
            asset: "USDC".into(),
            network: "solana".into(),
        }
    }

    #[test]
    fn parses_flat_challenge_body() {
        let body = serde_json::json!({
            "amount": "25.00",
            "recipient": "RecipientAddr",
            "asset": "USDC",
            "network": "solana"
        });
        assert_eq!(X402Challenge::from_body(&body).unwrap(), sample_challenge());
    }

    #[test]
    fn parses_accepts_envelope_with_aliases() {
        let body = serde_json::json!({
            "accepts": [ { "maxAmountRequired": "10.00", "payTo": "Somebody" } ]
        });
        let ch = X402Challenge::from_body(&body).unwrap();
        assert_eq!(ch.amount, "10.00");
        assert_eq!(ch.recipient, "Somebody");
        assert_eq!(ch.asset, "USDC");
        assert_eq!(ch.network, "solana");
    }

    #[test]
    fn missing_amount_is_an_error() {
        let body = serde_json::json!({ "recipient": "x" });
        assert!(X402Challenge::from_body(&body).is_err());
    }

    #[test]
    fn authorize_signs_a_verifiable_payload() {
        let signer = LocalSigner::generate();
        let ch = sample_challenge();
        let auth = authorize(&signer, &ch, 1_700_000_000);

        assert_eq!(auth.agent_id, signer.agent_id());
        assert_eq!(auth.amount, "25.00");
        assert_eq!(auth.recipient, "RecipientAddr");
        verify(&auth).expect("authorization verifies against its own key");
    }

    #[test]
    fn authorize_upto_carries_the_cap() {
        let signer = LocalSigner::generate();
        let ch = sample_challenge();
        let auth = authorize_upto(&signer, &ch, "100.00", 1_700_000_000);
        assert_eq!(auth.amount, "100.00");
        verify(&auth).expect("upto authorization verifies");
    }

    #[test]
    fn tampered_authorization_fails_verification() {
        let signer = LocalSigner::generate();
        let ch = sample_challenge();
        let mut auth = authorize(&signer, &ch, 1_700_000_000);
        auth.amount = "0.01".into();
        assert!(
            verify(&auth).is_err(),
            "changed amount must break the signature"
        );
    }

    /// A deterministic source, for asserting minting is a pure function of its
    /// bytes. Never use anything like this outside tests.
    struct FixedTokens(u8);

    impl TokenSource for FixedTokens {
        fn fill(&self, out: &mut [u8]) {
            out.fill(self.0);
        }
    }

    #[test]
    fn nonce_is_a_pure_function_of_the_source_bytes() {
        // The property, not the encoding: the nonce is the CSPRNG's output and
        // nothing else. If a clock or a counter were mixed in, two mints from
        // the same bytes would differ.
        assert_eq!(
            mint_nonce(&FixedTokens(0xAB)),
            mint_nonce(&FixedTokens(0xAB))
        );
        assert_ne!(
            mint_nonce(&FixedTokens(0xAB)),
            mint_nonce(&FixedTokens(0xCD))
        );
    }

    #[test]
    fn nonces_minted_in_the_same_millisecond_differ() {
        let signer = LocalSigner::generate();
        let ch = sample_challenge();
        let mut seen = HashSet::new();
        // A tight loop lands many mints inside one millisecond, which is
        // exactly where a clock-prefixed id has only its counter left.
        for _ in 0..1000 {
            let auth = authorize(&signer, &ch, 1_700_000_000);
            assert!(seen.insert(auth.nonce), "a nonce repeated");
        }
    }

    #[test]
    fn nonces_carry_no_monotonic_counter() {
        let signer = LocalSigner::generate();
        let ch = sample_challenge();
        let minted: Vec<String> = (0..64)
            .map(|_| authorize(&signer, &ch, 1_700_000_000).nonce)
            .collect();

        // An id built from a timestamp plus an incrementing counter sorts in
        // mint order. Random values do not: 64 draws land sorted with
        // probability 1/64!, so this failing means order leaked back in.
        assert!(
            minted.windows(2).any(|w| w[0] > w[1]),
            "nonces arrived in ascending order, which implies a counter"
        );

        // And no shared structure: a common prefix is what a clock component
        // would leave behind across mints in the same millisecond.
        let first = minted[0].as_bytes();
        assert!(
            minted[1..]
                .iter()
                .any(|n| n.as_bytes().first() != first.first()),
            "every nonce shared a leading byte, which implies a fixed prefix"
        );
    }

    #[test]
    fn nonce_is_url_safe_and_full_width() {
        let auth = authorize(&LocalSigner::generate(), &sample_challenge(), 1_700_000_000);
        // 32 bytes unpadded base64url.
        assert_eq!(auth.nonce.len(), 43, "unexpected nonce: {}", auth.nonce);
        assert!(
            auth.nonce
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "nonce is not base64url: {}",
            auth.nonce
        );
    }

    #[test]
    fn a_changed_nonce_breaks_the_signature() {
        // The nonce is inside the signed payload, so replaying an
        // authorization under a fresh nonce is not something a payer can do
        // without the key.
        let signer = LocalSigner::generate();
        let mut auth = authorize(&signer, &sample_challenge(), 1_700_000_000);
        auth.nonce = mint_nonce(&OsTokens);
        assert!(
            verify(&auth).is_err(),
            "changed nonce must break the signature"
        );
    }

    #[test]
    fn authorization_json_round_trips() {
        let signer = LocalSigner::generate();
        let auth = authorize(&signer, &sample_challenge(), 1_700_000_000);
        let json = serde_json::to_string(&auth).expect("serialize");
        assert!(json.contains("\"agentId\""));
        assert!(json.contains("\"signature\""));
        let back: X402Authorization = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, auth);
    }
}
