//! Proving a runner is who it says, and that somebody authorised it.
//!
//! Two separate questions, and conflating them is the mistake this module
//! exists to avoid:
//!
//! 1. **Is this the keypair it claims?** Answered by a signature over a
//!    server-issued challenge. Stops replay and impersonation.
//! 2. **Is it allowed to act for an owner?** Answered by an *owner
//!    attestation*: a signature by the **owner's** key over a statement naming
//!    the runner. Stops a valid-but-unauthorised runner attaching itself.
//!
//! A runner that answers only the first is authentic and entitled to nothing.
//!
//! ## Ed25519, not secp256k1
//!
//! Buzz's equivalent (NIP-OA) is Schnorr over secp256k1 because it lives in a
//! nostr world. Importing a second curve for fidelity to someone else's
//! ecosystem would mean a second signing implementation to keep correct.
//! `crypto::ed25519` is already Ed25519, already reviewed, and already has the
//! base58 key handling — so this reuses it and only adds domain separation.
//!
//! ## Why the domain tags matter
//!
//! Every signature here is over bytes that begin with a tag naming what is
//! being signed. Without one, a signature collected in one context is a valid
//! signature in another: an owner attestation could be replayed as a runner
//! handshake, or a wallet sign-in signature as either. The tags make each
//! keyspace disjoint, which is the same reasoning behind the pairing-code hash
//! prefix in `users::devices`.

use crate::Result;
use crate::error::OpenCompanyError;

/// Domain tag for a runner proving possession of its key.
const RUNNER_DOMAIN: &str = "opencompany-runner-v1";
/// Domain tag for an owner authorising a runner.
const OWNER_DOMAIN: &str = "opencompany-owner-attestation-v1";

/// How far a runner's clock may be from the host's.
///
/// Five minutes either way. A tighter one breaks laptops whose clock drifts
/// between sleeps; a looser one widens the replay window a challenge already
/// closes.
pub const SKEW_SECS: i64 = 300;

/// An owner's statement that a runner may act for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerAttestation {
    /// The owner's public key, base58.
    pub owner: String,
    /// What the runner is permitted to do, as an opaque canonical string —
    /// companies, an expiry, a parallelism cap.
    pub conditions: String,
    /// The owner's signature over [`owner_canonical_bytes`].
    pub signature: String,
}

/// A runner's opening message.
#[derive(Clone, Debug)]
pub struct RunnerHello {
    /// The runner's public key, base58. Its identity everywhere.
    pub runner_id: String,
    /// The challenge this host issued for this connection.
    pub challenge: String,
    /// Seconds since the epoch, as the runner sees it.
    pub timestamp: i64,
    /// A hash of what the runner advertised, so its capabilities are signed
    /// rather than merely asserted alongside a signature.
    pub capabilities_hash: String,
    /// The runner's signature over [`runner_canonical_bytes`].
    pub signature: String,
    pub attestation: OwnerAttestation,
}

/// The bytes a runner signs.
///
/// Includes the challenge (so a signature cannot be replayed onto another
/// connection) and the capabilities hash (so what it claims to be able to do is
/// covered by the signature rather than sitting next to it, editable).
pub fn runner_canonical_bytes(
    runner_id: &str,
    challenge: &str,
    timestamp: i64,
    capabilities_hash: &str,
) -> Vec<u8> {
    format!("{RUNNER_DOMAIN}\n{runner_id}\n{challenge}\n{timestamp}\n{capabilities_hash}")
        .into_bytes()
}

/// The bytes an owner signs to authorise a runner.
///
/// Note what is *not* here: the owner's private key never leaves the owner, and
/// this grants no ability to sign as them. It is provenance — "this runner acts
/// for me" — not delegation.
pub fn owner_canonical_bytes(owner: &str, runner_id: &str, conditions: &str) -> Vec<u8> {
    format!("{OWNER_DOMAIN}\n{owner}\n{runner_id}\n{conditions}").into_bytes()
}

/// Verifies a hello: skew, runner signature, then owner attestation.
///
/// Ordered cheapest-first, and the order is also the useful one for an
/// operator: a clock problem is reported as a clock problem rather than as a
/// bad signature.
///
/// The caller must additionally reject a replayed `challenge` — this function
/// cannot, because it holds no state. `NonceCache` is the intended companion.
pub fn verify_hello(hello: &RunnerHello, now: i64) -> Result<()> {
    if (now - hello.timestamp).abs() > SKEW_SECS {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "runner timestamp is outside the ±{SKEW_SECS}s window"
        )));
    }

    let runner_bytes = runner_canonical_bytes(
        &hello.runner_id,
        &hello.challenge,
        hello.timestamp,
        &hello.capabilities_hash,
    );
    crate::crypto::ed25519::verify_b58(&hello.runner_id, &runner_bytes, &hello.signature)
        .map_err(|_| OpenCompanyError::InvalidRequest("runner signature does not verify".into()))?;

    // Authentic is not the same as authorised. A runner that passes the check
    // above and fails this one has proved it is itself and nothing more.
    let owner_bytes = owner_canonical_bytes(
        &hello.attestation.owner,
        &hello.runner_id,
        &hello.attestation.conditions,
    );
    crate::crypto::ed25519::verify_b58(
        &hello.attestation.owner,
        &owner_bytes,
        &hello.attestation.signature,
    )
    .map_err(|_| OpenCompanyError::InvalidRequest("owner attestation does not verify".into()))?;

    Ok(())
}

#[cfg(test)]
#[path = "attest_tests.rs"]
mod tests;
