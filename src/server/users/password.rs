//! Password hashing, verification, and policy.
//!
//! Passwords are an *optional* convenience alongside the magic link: a user may
//! set one to log in without waiting for mail, and a user who never sets one is
//! unaffected. [`UserRecord::password_hash`](crate::ports::UserRecord) is
//! `None` for them.
//!
//! ## Argon2id, and why the parameters are not the library defaults' business
//!
//! Hashes are Argon2id in PHC string format (`$argon2id$v=19$m=...`), which
//! embeds the algorithm, version, parameters, and salt. That is what lets
//! [`verify`] keep validating old hashes after the cost parameters are raised —
//! each hash carries the parameters it was made with.
//!
//! ## Two timing concerns, both real
//!
//! - **Verification** must not leak the password a byte at a time. `argon2`'s
//!   `verify_password` compares digests in constant time; nothing here compares
//!   a password with `==`.
//! - **Absence** must not leak. Verifying against a real hash takes ~50ms;
//!   returning early for an unknown email takes ~0ms. That difference is a
//!   user-enumeration oracle, which the magic-link path is careful not to
//!   provide, so the password path must not hand one back. [`dummy_verify`]
//!   burns the same work for an address with no account or no password.
//!
//! ## What is deliberately absent
//!
//! No composition rules (no "one uppercase, one digit"). NIST SP 800-63B
//! recommends against them: they push people toward `Password1!` and add no
//! entropy worth the friction. Length is the check that matters.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

use crate::error::OpenCompanyError;
use crate::server::users::token::TokenSource;

/// The shortest password accepted.
///
/// Above NIST's floor of 8. These accounts reach a company's chat, tasks, and
/// workspace, and the magic link is always available for anyone who would
/// rather not have a password at all.
pub const MIN_PASSWORD_LEN: usize = 12;

/// The longest password accepted.
///
/// Not a security limit — Argon2 has no meaningful input ceiling — but an
/// unbounded field is free CPU for whoever posts a megabyte to the login route.
pub const MAX_PASSWORD_LEN: usize = 512;

/// Bytes of salt per hash. 16 is the PHC/Argon2 recommendation.
const SALT_BYTES: usize = 16;

/// Checks a candidate password against policy.
///
/// `email` is compared against so that a password that is merely the account's
/// own address is refused — it is public, and it is the first thing anyone
/// tries.
pub fn validate(password: &str, email: &str) -> Result<(), OpenCompanyError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "a password may be at most {MAX_PASSWORD_LEN} bytes"
        )));
    }
    if password.trim().is_empty() {
        return Err(OpenCompanyError::InvalidRequest(
            "a password cannot be only whitespace".to_string(),
        ));
    }
    // Both sides trimmed: padding the address with spaces must not smuggle it
    // past this. The password itself is still stored untrimmed — leading and
    // trailing spaces are legitimate characters in a passphrase.
    if password.trim().eq_ignore_ascii_case(email.trim()) {
        return Err(OpenCompanyError::InvalidRequest(
            "a password cannot be your email address".to_string(),
        ));
    }
    Ok(())
}

/// Hashes `password` with Argon2id, returning a PHC string safe to store.
///
/// The salt comes from the crate's [`TokenSource`] rather than argon2's own RNG
/// feature, so there is one source of randomness in the process and tests can
/// make hashing deterministic.
pub fn hash(src: &dyn TokenSource, password: &str) -> Result<String, OpenCompanyError> {
    let mut salt_bytes = [0u8; SALT_BYTES];
    src.fill(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| OpenCompanyError::Store(format!("password salt: {e}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| OpenCompanyError::Store(format!("password hash: {e}")))
}

/// Whether `password` matches the stored PHC `phc` hash.
///
/// Returns `false` — never an error — for a malformed stored hash too: a
/// corrupt record must fail closed as a wrong password, not 500 in a way that
/// distinguishes it.
pub fn verify(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Burns the same work a real [`verify`] would, and discards it.
///
/// Call on every login where there is no hash to check — unknown address,
/// known address with no password set, suspended user — so that "no account
/// here" costs the same wall-clock as "wrong password". Without it, response
/// time answers the question the generic error message refuses to.
pub fn dummy_verify(password: &str) {
    // A fixed, valid Argon2id hash of a value nothing can log in with. Its
    // parameters match `Argon2::default()`, so the work matches a real verify.
    const DUMMY_PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$\
                             c29tZXNhbHRzb21lc2FsdA$\
                             Ik8jitpTS4/1sMkKY0YMlUj3PYm3W2v0wNKPRLGSaBM";
    let _ = verify(password, DUMMY_PHC);
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::server::users::token::OsTokens;

    struct FixedTokens(u8);
    impl TokenSource for FixedTokens {
        fn fill(&self, out: &mut [u8]) {
            out.fill(self.0);
        }
    }

    #[test]
    fn a_hash_round_trips_and_rejects_the_wrong_password() {
        let phc = hash(&OsTokens, "correct horse battery").unwrap();
        assert!(verify("correct horse battery", &phc));
        assert!(!verify("Correct horse battery", &phc), "case must matter");
        assert!(!verify("wrong", &phc));
        assert!(!verify("", &phc));
    }

    #[test]
    fn a_stored_hash_is_argon2id_and_never_the_password() {
        let phc = hash(&OsTokens, "correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        assert!(
            !phc.contains("correct horse battery"),
            "the password leaked into its own hash: {phc}"
        );
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // Distinct salts: two users with the same password must not share a
        // hash, or one crack breaks both and the store leaks who matches whom.
        let a = hash(&OsTokens, "correct horse battery").unwrap();
        let b = hash(&OsTokens, "correct horse battery").unwrap();
        assert_ne!(a, b);
        assert!(verify("correct horse battery", &a));
        assert!(verify("correct horse battery", &b));
    }

    #[test]
    fn the_salt_comes_from_the_token_source() {
        // Proves hashing draws from the crate's one randomness seam rather than
        // argon2's own RNG — otherwise this would differ.
        let a = hash(&FixedTokens(7), "correct horse battery").unwrap();
        let b = hash(&FixedTokens(7), "correct horse battery").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, hash(&FixedTokens(9), "correct horse battery").unwrap());
    }

    #[test]
    fn a_malformed_stored_hash_fails_closed() {
        for junk in ["", "not-a-hash", "$argon2id$broken", "$2y$10$bcryptish"] {
            assert!(
                !verify("anything", junk),
                "{junk:?} must not verify as a password"
            );
        }
    }

    #[test]
    fn the_dummy_hash_is_valid_work_nobody_can_log_in_with() {
        // If DUMMY_PHC were malformed, verify() would bail immediately and the
        // timing equalization it exists for would silently do nothing.
        dummy_verify("anything");
        const DUMMY_PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$\
                                 c29tZXNhbHRzb21lc2FsdA$\
                                 Ik8jitpTS4/1sMkKY0YMlUj3PYm3W2v0wNKPRLGSaBM";
        assert!(
            PasswordHash::new(DUMMY_PHC).is_ok(),
            "the dummy hash must parse, or it does no work and leaks timing"
        );
    }

    #[test]
    fn policy_enforces_length_but_not_composition() {
        let email = "ada@example.com";
        // Long enough, no uppercase/digit/symbol: accepted on purpose.
        assert!(validate("correct horse battery", email).is_ok());
        assert!(validate(&"a".repeat(MIN_PASSWORD_LEN), email).is_ok());

        let err = validate(&"a".repeat(MIN_PASSWORD_LEN - 1), email).unwrap_err();
        assert_eq!(err.code(), "invalid_request");
        assert!(format!("{err}").contains("12"));

        assert!(validate(&"a".repeat(MAX_PASSWORD_LEN + 1), email).is_err());
        assert!(validate("            ", email).is_err(), "whitespace only");
    }

    #[test]
    fn policy_counts_characters_not_bytes() {
        // 12 multi-byte characters is 12 characters. Counting bytes would let a
        // shorter password through and reject a legitimate one.
        let emoji = "🔑".repeat(MIN_PASSWORD_LEN);
        assert!(validate(&emoji, "ada@example.com").is_ok());
        let short = "🔑".repeat(MIN_PASSWORD_LEN - 1);
        assert!(validate(&short, "ada@example.com").is_err());
    }

    #[test]
    fn a_password_cannot_be_the_account_email() {
        let email = "ada.lovelace@example.com";
        assert!(validate(email, email).is_err());
        assert!(validate("Ada.Lovelace@Example.com", email).is_err());
        assert!(validate(" ada.lovelace@example.com ", email).is_err());
    }
}
