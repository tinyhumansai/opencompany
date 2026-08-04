//! Dependency-free identifier and timestamp sources for the runtime.
//!
//! Phase 1 avoids pulling `uuid`/`ulid`/`chrono`. Minted string ids combine an
//! epoch-millis prefix with a process-global monotonic counter so they are
//! collision-safe in-process, human-readable in JSONL, and lexicographically
//! monotonic (both components are zero-padded hex).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Current wall-clock time as epoch milliseconds.
///
/// Returns `0` if the system clock is set before the Unix epoch (never in
/// practice); callers treat the value as an opaque monotonic-ish stamp.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mints a fresh process-unique id of the form `{millis:012x}-{counter:012x}`.
///
/// The counter is strictly increasing, so two calls always differ and — given
/// a non-decreasing clock — sort in mint order.
///
/// # Uniqueness is process-local
///
/// `COUNTER` starts at zero in every process and the millis prefix has
/// millisecond resolution, so two processes that start within the same
/// millisecond mint *identical* ids. Never use a minted id to name an entry in
/// a directory other processes share — `/tmp` above all. Tests that need a
/// private path must take one from `tempfile` (`tempfile::Builder::new()
/// .prefix("opencompany-…").tempdir()`), which asks the OS for a name no other
/// process can hold, rather than deriving one from `generate_id`.
///
/// # Never where unpredictability is required
///
/// A minted id is fully guessable from a prior one: the counter steps by one
/// and the prefix is the wall clock. It must not be used for a token, a
/// secret, a capability URL, or a nonce — anything whose safety rests on a
/// reader being unable to name the next value. Those come from the OS CSPRNG
/// through [`TokenSource`](crate::server::users::token::TokenSource); see
/// [`mint_session_token`](crate::server::users::token::mint_session_token) and
/// the x402 authorization nonce for the two shapes already in the tree.
pub fn generate_id() -> String {
    let millis = now_millis();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{millis:012x}-{counter:012x}")
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn generated_ids_are_distinct_and_monotonic() {
        let a = generate_id();
        let b = generate_id();
        assert_ne!(a, b);
        // Zero-padded fixed-width hex makes lexicographic order match mint order.
        assert!(b > a, "expected {b} > {a}");
    }
}
