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

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::economy::signer::{LocalSigner, verify_b58};
use crate::error::OpenCompanyError;
use crate::ports::generate_id;

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
    /// A single-use nonce.
    pub nonce: String,
    /// The authorization timestamp, epoch seconds.
    pub timestamp: i64,
    /// The base58 Ed25519 signature over [`canonical_bytes`].
    #[serde(rename = "signature")]
    pub signature_b58: String,
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
    let nonce = generate_id();
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
