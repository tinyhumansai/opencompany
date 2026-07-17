//! Minting and hashing the two secrets in the user-auth flow.
//!
//! There are exactly two: the **login code** carried in a magic link, and the
//! **session token** carried in a cookie. Both follow the same three rules.
//!
//! ## 1. They come from the OS CSPRNG, never from `generate_id`
//!
//! [`generate_id`](crate::ports::generate_id) is epoch-millis plus a counter —
//! excellent for record ids, and completely predictable. Anyone who can guess
//! roughly when a link was minted could enumerate its id. Secrets come from
//! [`TokenSource`] instead.
//!
//! ## 2. Only their hashes are stored
//!
//! The plaintext exists in exactly one place — the email that was sent, or the
//! browser's cookie jar — and is never written down. The stores hold
//! [`sha256_hex`] output, so a dump of the database (or of a backup, or of an
//! export) cannot be replayed as anyone.
//!
//! ## 3. Lookup is *by* the hash, so nothing is ever compared
//!
//! Both stores find a record by hashing what the caller presented and looking
//! that up. There is no "fetch the record, then compare the secret" step, which
//! is why this module has no constant-time comparison: there is no comparison.
//! Forging a hit would require a SHA-256 preimage.
//!
//! That property is bought with entropy, and it is why the login code is a
//! 256-bit token in a link rather than six digits to type. A six-digit code
//! *must* be looked up by email and compared, which then needs constant-time
//! equality, an attempt budget, and an atomic counter in all three backends to
//! resist a 10⁶ brute force. The link avoids all of it.

use sha2::{Digest, Sha256};

use crate::server::platform_auth::b64url_encode;

/// How long a magic link stays redeemable.
///
/// Short, because the link *is* the credential: it sits in a mailbox, may be
/// forwarded, and lands in the browser history of whoever clicks it. Long
/// enough to survive mail-delivery lag and a distracted human.
pub const LOGIN_CODE_TTL_MILLIS: u64 = 15 * 60 * 1000;

/// How long a session stays valid once minted.
///
/// Absolute, not sliding: extending on every request would mean a store write
/// per request, which on the fs backend is a whole-file rewrite. Revocation is
/// the lever for cutting a session short, not expiry.
pub const SESSION_TTL_MILLIS: u64 = 14 * 24 * 60 * 60 * 1000;

/// How many random bytes back each secret. 32 bytes = 256 bits, which is why
/// guessing is not a threat model and the codes need no attempt budget.
const TOKEN_BYTES: usize = 32;

/// A source of cryptographically secure random bytes.
///
/// A seam, so tests can mint reproducible tokens without `unsafe` or global
/// state. Production always uses [`OsTokens`].
pub trait TokenSource: Send + Sync {
    /// Fills `out` with unpredictable bytes.
    fn fill(&self, out: &mut [u8]);
}

/// The real source: the operating system's CSPRNG.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsTokens;

impl TokenSource for OsTokens {
    fn fill(&self, out: &mut [u8]) {
        // A CSPRNG failure means the OS cannot give us randomness. There is no
        // safe degraded behavior — falling back to anything predictable would
        // hand out forgeable credentials — so refuse loudly instead.
        getrandom::getrandom(out).expect("the OS CSPRNG is unavailable; cannot mint a secret");
    }
}

/// Mints an opaque session token: 256 bits, base64url, 43 chars.
///
/// Returned to the browser once and never stored; persist
/// [`sha256_hex`] of it instead.
pub fn mint_session_token(src: &dyn TokenSource) -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    src.fill(&mut bytes);
    b64url_encode(&bytes)
}

/// Mints a login code for a magic link: 256 bits, base64url, 43 chars.
///
/// Deliberately the same shape as a session token. It is URL-safe with no
/// escaping, which matters because it is pasted straight into a link.
pub fn mint_login_code(src: &dyn TokenSource) -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    src.fill(&mut bytes);
    b64url_encode(&bytes)
}

/// Lowercase-hex SHA-256 of `input`. The only form of a secret that is stored.
pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        // Infallible: writing to a String never fails.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use super::*;

    /// A deterministic source, for asserting minting is a pure function of its
    /// bytes. Never use anything like this outside tests.
    struct FixedTokens(u8);

    impl TokenSource for FixedTokens {
        fn fill(&self, out: &mut [u8]) {
            out.fill(self.0);
        }
    }

    #[test]
    fn sha256_hex_matches_the_published_vector() {
        // The canonical FIPS 180-2 test vector for "abc".
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hashing_is_deterministic_and_diffuses() {
        assert_eq!(sha256_hex("token"), sha256_hex("token"));
        // One-character difference, completely different digest.
        assert_ne!(sha256_hex("token"), sha256_hex("tokem"));
    }

    #[test]
    fn minted_tokens_are_unpredictable_and_url_safe() {
        let src = OsTokens;
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let token = mint_session_token(&src);
            // 32 bytes unpadded base64url.
            assert_eq!(token.len(), 43, "unexpected token length: {token}");
            assert!(
                token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "token is not base64url and would need escaping in a link: {token}"
            );
            assert!(seen.insert(token), "the CSPRNG repeated a token");
        }
    }

    #[test]
    fn login_codes_are_also_unpredictable() {
        let src = OsTokens;
        let a = mint_login_code(&src);
        let b = mint_login_code(&src);
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn minting_is_a_pure_function_of_the_source_bytes() {
        // Proves the token is the CSPRNG's output and nothing else — no clock,
        // no counter, no process state leaking in.
        let a = mint_session_token(&FixedTokens(0xAB));
        let b = mint_session_token(&FixedTokens(0xAB));
        assert_eq!(a, b);
        assert_ne!(a, mint_session_token(&FixedTokens(0xCD)));
    }

    #[test]
    fn a_minted_token_is_never_its_own_hash() {
        // Guards the one mistake that would defeat the whole scheme: storing
        // the plaintext under a field named `*_hash`.
        let token = mint_session_token(&OsTokens);
        assert_ne!(token, sha256_hex(&token));
        assert_eq!(sha256_hex(&token).len(), 64);
    }
}
