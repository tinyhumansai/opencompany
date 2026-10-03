use super::*;

#[test]
fn agent_id_is_base58_of_the_32_byte_public_key() {
    let signer = LocalSigner::from_seed(&[7; 32]);
    let decoded = bs58::decode(signer.agent_id()).into_vec().expect("base58");
    assert_eq!(decoded.len(), 32);
}

#[test]
fn sign_verify_round_trip_and_wrong_key_fails() {
    let signer = LocalSigner::from_seed(&[1; 32]);
    let other = LocalSigner::from_seed(&[2; 32]);
    let msg = b"canonical-payload";

    let sig = signer.sign_b58(msg);
    verify_b58(&signer.agent_id(), msg, &sig).expect("valid signature verifies");

    // Same signature attributed to a different key must fail.
    assert!(verify_b58(&other.agent_id(), msg, &sig).is_err());
    // Tampered message must fail.
    assert!(verify_b58(&signer.agent_id(), b"tampered", &sig).is_err());
}

#[test]
fn malformed_key_or_signature_is_refused_not_panicked() {
    let signer = LocalSigner::from_seed(&[3; 32]);
    let sig = signer.sign_b58(b"m");
    // Not base58 at all (`0` is outside the alphabet).
    assert!(verify_b58("0000", b"m", &sig).is_err());
    // Valid base58, wrong length.
    assert!(verify_b58(&bs58::encode([1u8; 31]).into_string(), b"m", &sig).is_err());
    assert!(verify_b58(&signer.agent_id(), b"m", "abc").is_err());
}
