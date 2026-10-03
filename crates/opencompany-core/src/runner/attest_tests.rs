use super::*;
use crate::crypto::ed25519::LocalSigner;

struct Key {
    signer: LocalSigner,
    id: String,
}

fn key(seed: u8) -> Key {
    let signer = LocalSigner::from_seed(&[seed; 32]);
    let id = signer.agent_id();
    Key { signer, id }
}

fn hello(runner: &Key, owner: &Key, now: i64) -> RunnerHello {
    let conditions = "companies=acme&max_parallel=2";
    let attestation = OwnerAttestation {
        owner: owner.id.clone(),
        conditions: conditions.to_string(),
        signature: owner
            .signer
            .sign_b58(&owner_canonical_bytes(&owner.id, &runner.id, conditions)),
    };
    let bytes = runner_canonical_bytes(&runner.id, "chal-1", now, "caps-hash");
    RunnerHello {
        runner_id: runner.id.clone(),
        challenge: "chal-1".to_string(),
        timestamp: now,
        capabilities_hash: "caps-hash".to_string(),
        signature: runner.signer.sign_b58(&bytes),
        attestation,
    }
}

#[test]
fn a_well_formed_hello_verifies() {
    let (runner, owner) = (key(1), key(2));
    assert!(verify_hello(&hello(&runner, &owner, 1_000), 1_000).is_ok());
}

#[test]
fn a_stale_clock_is_reported_as_a_clock_problem() {
    // Ordered first so an operator whose laptop slept gets told that,
    // rather than "bad signature" — which would send them looking at keys.
    let (runner, owner) = (key(1), key(2));
    let h = hello(&runner, &owner, 1_000);
    let error = verify_hello(&h, 1_000 + SKEW_SECS + 1).unwrap_err();
    assert!(error.to_string().contains("window"), "{error}");
}

#[test]
fn a_signature_for_another_challenge_is_refused() {
    // The replay this exists to stop: a hello captured on one connection
    // re-presented on another.
    let (runner, owner) = (key(1), key(2));
    let mut h = hello(&runner, &owner, 1_000);
    h.challenge = "a-different-challenge".to_string();
    assert!(verify_hello(&h, 1_000).is_err());
}

#[test]
fn advertised_capabilities_are_covered_by_the_signature() {
    // Otherwise a runner's claims sit *next to* a valid signature and can be
    // edited in flight — a man in the middle could advertise harnesses the
    // machine does not have, and the host would schedule to them.
    let (runner, owner) = (key(1), key(2));
    let mut h = hello(&runner, &owner, 1_000);
    h.capabilities_hash = "tampered".to_string();
    assert!(verify_hello(&h, 1_000).is_err());
}

#[test]
fn a_runner_signing_as_someone_else_is_refused() {
    let (runner, owner, impostor) = (key(1), key(2), key(3));
    let mut h = hello(&runner, &owner, 1_000);
    h.runner_id = impostor.id.clone();
    assert!(verify_hello(&h, 1_000).is_err());
}

#[test]
fn an_authentic_runner_with_no_owner_authorisation_is_still_refused() {
    // THE distinction. This runner genuinely holds its key — it just has
    // nobody's permission to attach itself to this host.
    let (runner, owner, other) = (key(1), key(2), key(3));
    let mut h = hello(&runner, &owner, 1_000);
    // An attestation signed by a different key than it names.
    h.attestation.signature = other.signer.sign_b58(&owner_canonical_bytes(
        &owner.id,
        &runner.id,
        &h.attestation.conditions,
    ));

    let error = verify_hello(&h, 1_000).unwrap_err();
    assert!(error.to_string().contains("attestation"), "{error}");
}

#[test]
fn an_attestation_for_a_different_runner_does_not_transfer() {
    // Lifting someone else's valid attestation is the obvious attack once
    // attestations exist at all.
    let (runner, owner, other_runner) = (key(1), key(2), key(4));
    let conditions = "companies=acme&max_parallel=2";
    let mut h = hello(&runner, &owner, 1_000);
    h.attestation.signature = owner.signer.sign_b58(&owner_canonical_bytes(
        &owner.id,
        &other_runner.id,
        conditions,
    ));
    assert!(verify_hello(&h, 1_000).is_err());
}

#[test]
fn conditions_cannot_be_widened_after_signing() {
    // The conditions carry the companies and the parallelism cap; if they
    // were not signed, a runner could grant itself the whole host.
    let (runner, owner) = (key(1), key(2));
    let mut h = hello(&runner, &owner, 1_000);
    h.attestation.conditions = "companies=*&max_parallel=999".to_string();
    assert!(verify_hello(&h, 1_000).is_err());
}

#[test]
fn the_two_domains_are_disjoint_keyspaces() {
    // Without the tags, a signature collected in one context verifies in
    // the other — an owner attestation replayed as a runner handshake.
    let runner_bytes = runner_canonical_bytes("r", "c", 1, "h");
    let owner_bytes = owner_canonical_bytes("o", "r", "c");
    assert_ne!(runner_bytes, owner_bytes);
    assert!(String::from_utf8_lossy(&runner_bytes).starts_with(RUNNER_DOMAIN));
    assert!(String::from_utf8_lossy(&owner_bytes).starts_with(OWNER_DOMAIN));
}
