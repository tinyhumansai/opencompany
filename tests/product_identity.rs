//! Issue #376 acceptance: the embedded `openhuman_core` must attribute this
//! process's backend traffic to `opencompany`, not to its own `openhuman`
//! default.
//!
//! This lives in `tests/` rather than beside the code because the identity is
//! **process-global** state in core (a `OnceLock<RwLock<_>>`). An in-crate
//! `#[cfg(test)]` test would share that global with every other unit test in
//! the same binary, running on parallel threads: asserting "the default is
//! still `openhuman`" would race any test that had already installed an
//! override, and the failure would look like a flake rather than the ordering
//! bug it is. An integration target gets its own process, so the
//! before/after sequence below is meaningful and cannot be perturbed.
//!
//! Gated on `openhuman` alone, deliberately: `scripts/ci/assert-integration-
//! targets-run.sh` asserts a NON-ZERO test count for every target under
//! `tests/` in the `openhuman,tinycortex` lane, so a target gated on a feature
//! that lane lacks would compile to an empty binary and fail that check rather
//! than silently guarding nothing.
#![cfg(feature = "openhuman")]

use opencompany::product::PRODUCT_IDENTITY;
use openhuman_core::api::{DEFAULT_PRODUCT_IDENTITY, ProductIdentity, product_identity};

/// The acceptance criterion itself, as a before/after pair.
///
/// Asserting the pre-install state is the half that carries the weight: without
/// it, a regression that dropped the `install_into_embedded_core` call entirely
/// would still pass if `DEFAULT_PRODUCT_IDENTITY` ever happened to be
/// `"opencompany"`. Proving the value *changed* from the inherited default is
/// what proves the wiring runs.
///
/// Kept as one test function rather than split: the two halves are ordered
/// against shared process state, and `cargo test` runs separate functions on
/// parallel threads.
#[test]
fn install_replaces_the_inherited_openhuman_default_with_opencompany() {
    assert_eq!(
        product_identity().as_str(),
        DEFAULT_PRODUCT_IDENTITY,
        "before install, the embedded core should still be on its own default"
    );

    opencompany::product::install_into_embedded_core();

    assert_eq!(
        product_identity().as_str(),
        "opencompany",
        "after install, backend traffic must be attributed to opencompany"
    );
    assert_ne!(
        product_identity().as_str(),
        DEFAULT_PRODUCT_IDENTITY,
        "the inherited openhuman default must not survive install"
    );
}

/// Core's `ProductIdentity::new` sanitises against an allowlist and returns
/// `None` when nothing usable survives. `install_into_embedded_core` can only
/// warn in that case, leaving traffic on the `openhuman` default — so pin that
/// our constant actually clears the sanitiser instead of relying on it.
///
/// Touches no process state, so it is safe to run alongside the test above.
#[test]
fn product_identity_survives_the_embedded_core_sanitiser() {
    let sanitised = ProductIdentity::new(PRODUCT_IDENTITY)
        .expect("opencompany must survive core's identity sanitiser");
    assert_eq!(
        sanitised.as_str(),
        PRODUCT_IDENTITY,
        "core's sanitiser must not rewrite our identity (it lower-cases and \
         strips to [0-9A-Za-z._-], both of which this constant already satisfies)"
    );
}
