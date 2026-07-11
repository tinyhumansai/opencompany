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
