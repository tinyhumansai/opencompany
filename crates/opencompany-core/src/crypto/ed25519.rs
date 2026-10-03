//! Ed25519 signature verification over base58 (Solana-style) keys.
//!
//! An identity here is the base58 encoding of a 32-byte Ed25519 public key,
//! and a signature is the base58 encoding of its 64 bytes. Two callers verify
//! against that shape: wallet sign-in (`server::users::wallet`), where the key
//! is the user's wallet address, and the runner handshake (`runner::attest`),
//! where it is the runner's and the owner's id. Both go through
//! [`verify_b58`] so they cannot disagree about what verifies.
//!
//! Everything here is offline: verification never touches the network.

use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

use crate::Result;
use crate::error::OpenCompanyError;

/// Verifies a base58-encoded Ed25519 signature over `msg` by the base58
/// public key `key_b58`.
///
/// Returns `Ok(())` on a valid signature, or [`OpenCompanyError::InvalidRequest`]
/// when the key or signature is malformed or the signature does not verify.
pub fn verify_b58(key_b58: &str, msg: &[u8], signature_b58: &str) -> Result<()> {
    let pubkey_bytes = decode_32(key_b58).ok_or_else(|| {
        OpenCompanyError::InvalidRequest(format!("key `{key_b58}` is not a 32-byte base58 key"))
    })?;
    let verifying = VerifyingKey::from_bytes(&pubkey_bytes)
        .map_err(|_| OpenCompanyError::InvalidRequest("key is not a valid Ed25519 key".into()))?;

    let sig_bytes = decode_64(signature_b58).ok_or_else(|| {
        OpenCompanyError::InvalidRequest("signature is not 64 base58 bytes".into())
    })?;
    let signature = Signature::from_bytes(&sig_bytes);

    verifying
        .verify(msg, &signature)
        .map_err(|_| OpenCompanyError::InvalidRequest("signature does not verify".into()))
}

/// Decodes a base58 string that must hold exactly 32 bytes (a public key).
fn decode_32(b58: &str) -> Option<[u8; 32]> {
    bs58::decode(b58).into_vec().ok()?.try_into().ok()
}

/// Decodes a base58 string that must hold exactly 64 bytes (a signature).
fn decode_64(b58: &str) -> Option<[u8; 64]> {
    bs58::decode(b58).into_vec().ok()?.try_into().ok()
}

/// A deterministic Ed25519 signer for tests that need to produce signatures
/// [`verify_b58`] accepts — the runner handshake's, for one.
///
/// Test-only: production code verifies signatures and never makes them, so no
/// key material is generated or stored by the shipped binary.
#[cfg(test)]
pub(crate) struct LocalSigner {
    keypair: ed25519_dalek::SigningKey,
}

#[cfg(test)]
impl LocalSigner {
    /// Builds a signer from a raw 32-byte Ed25519 seed.
    pub(crate) fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            keypair: ed25519_dalek::SigningKey::from_bytes(seed),
        }
    }

    /// The base58 encoding of the 32-byte public key — the identity
    /// [`verify_b58`] takes.
    pub(crate) fn agent_id(&self) -> String {
        bs58::encode(self.keypair.verifying_key().to_bytes()).into_string()
    }

    /// Signs `msg` and returns the 64-byte signature base58-encoded.
    pub(crate) fn sign_b58(&self, msg: &[u8]) -> String {
        use ed25519_dalek::Signer as _;
        bs58::encode(self.keypair.sign(msg).to_bytes()).into_string()
    }
}

#[cfg(test)]
#[path = "ed25519_tests.rs"]
mod tests;
