//! This crate's product identity on the wire — the `x-sdk-name` header every
//! request this process makes to the TinyHumans backend must carry.
//!
//! OpenHuman, OpenCompany and Medulla share a single TinyHumans login, so the
//! backend cannot tell which product is calling from credentials alone; it
//! attributes traffic by reading this header (see
//! `openhuman_core::api::product`, whose doc comment names the backend-side
//! reader, `src/utils/sdkSource.ts` in `tinyhumansai/backend`). Without it, an
//! opencompany-driven request would be counted as `openhuman` — the vendored
//! runtime's own default — and OpenCompany usage would be invisible in
//! per-product attribution.
//!
//! The embedded `openhuman_core` crate (behind this crate's `openhuman`
//! feature) already solves this for its own outbound calls: a host calls
//! `openhuman_core::api::set_product_identity` once at startup and every
//! `IntegrationClient` it constructs afterward picks up the header
//! automatically. That covers `src/harness/toolbelt.rs`, `composio.rs` and
//! `search.rs` — but nothing else. This crate's *own* backend traffic
//! (feedback forwarding, the hosted inference/embeddings clients, the Medulla
//! HTTP transport) is built on its own `reqwest` clients, entirely bypassing
//! `openhuman_core`'s request path, so each of those call sites must attach
//! the header itself — that is what [`product_identity_header`] is for.
//!
//! This module is deliberately ungated (no `#[cfg(feature = "openhuman")]`)
//! and does NOT reference `openhuman_core` at all: `src/harness/` — the only
//! thing that talks to the embedded core — is feature-gated, but
//! `src/brain/` and `src/feedback/` are not, and both need this constant
//! regardless of which features are enabled.
//!
//! [`PRODUCT_IDENTITY`] is the single source of truth for the string
//! `"opencompany"` in this crate. It is deliberately duplicated nowhere else —
//! every call site that used to hold its own copy of that literal (notably
//! `feedback::tinyhumans::PRODUCT`) now re-exports this constant instead, so
//! the value can only ever drift by editing this one line.

/// This crate's product name as the backend's `x-sdk-name` enum expects it:
/// lower-case, ASCII, no separators beyond `.`/`_`/`-`. Every direct backend
/// client in this crate attaches it via [`product_identity_header`]; nothing
/// else in the crate should spell out `"opencompany"` as a literal.
pub const PRODUCT_IDENTITY: &str = "opencompany";

/// The header name the TinyHumans backend reads to attribute a request to a
/// product. Mirrors `openhuman_core::api::PRODUCT_IDENTITY_HEADER` exactly —
/// see the drift-guard test in this module's `tests`, which fails the build
/// the moment the two crates' header names would otherwise disagree.
pub const PRODUCT_IDENTITY_HEADER: &str = "x-sdk-name";

/// The `(name, value)` pair to attach to any outbound `reqwest::RequestBuilder`
/// via `.header(name, value)`, e.g.:
///
/// ```ignore
/// let (name, value) = opencompany::product::product_identity_header();
/// let request = client.post(url).header(name, value);
/// ```
///
/// Both halves are `'static` string slices rather than owned `String`s: the
/// identity is a compile-time constant, not something resolved per request,
/// so there is nothing to allocate.
pub fn product_identity_header() -> (&'static str, &'static str) {
    (PRODUCT_IDENTITY_HEADER, PRODUCT_IDENTITY)
}

/// Installs [`PRODUCT_IDENTITY`] as the embedded runtime's process-wide product
/// identity, so every backend client `openhuman_core` builds from here on is
/// tagged as opencompany rather than its own `openhuman` default.
///
/// **Call this exactly once, during startup, before anything constructs a
/// company runtime, agent harness or HTTP listener.** Core reads the identity
/// into a client's default headers *at construction*, so an
/// `IntegrationClient` that already exists when this runs keeps the old value
/// — the call does not retroactively re-tag anything.
///
/// This lives here rather than inline in `src/bin/opencompany.rs` so that the
/// wiring is reachable from a test. The binary's `Serve` arm is not, and an
/// untested one-line call is exactly where a product-attribution bug would sit
/// unnoticed: nothing about the process misbehaves when the identity is wrong,
/// the traffic is just silently counted as another product's.
#[cfg(feature = "openhuman")]
pub fn install_into_embedded_core() {
    match openhuman_core::api::ProductIdentity::new(PRODUCT_IDENTITY) {
        Some(identity) => {
            tracing::debug!(
                identity = identity.as_str(),
                "[product] installed the product identity into the embedded core"
            );
            openhuman_core::api::set_product_identity(identity);
        }
        // Unreachable while `PRODUCT_IDENTITY` is a lower-case ASCII literal —
        // core's sanitiser only returns `None` when nothing survives its
        // allowlist. Warn rather than ignore anyway: the failure mode is
        // silent, and a build that somehow reached here would ship every
        // request tagged `openhuman`, mis-attributing all of this product's
        // traffic with nothing else looking wrong. `product_identity_survives_
        // the_embedded_core_sanitiser` pins the reachable case.
        None => tracing::warn!(
            identity = PRODUCT_IDENTITY,
            "[product] the embedded core rejected our product identity; \
             backend traffic will be attributed to its default instead"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_opencompany() {
        assert_eq!(PRODUCT_IDENTITY, "opencompany");
    }

    #[test]
    fn header_pairs_the_sdk_name_key_with_the_identity() {
        assert_eq!(product_identity_header(), ("x-sdk-name", "opencompany"));
        assert_eq!(
            product_identity_header(),
            (PRODUCT_IDENTITY_HEADER, PRODUCT_IDENTITY)
        );
    }

    /// The whole point of this module existing separately from
    /// `openhuman_core::api::product`: the two crates' header name AND the
    /// fact that this crate's identity is NOT the embedded core's default
    /// must never be able to drift apart silently. If either assertion here
    /// ever fails, the embedded core changed its header contract (or its
    /// default happened to become "opencompany") without this crate noticing
    /// — exactly the kind of mismatch that would make the backend attribute
    /// OpenCompany traffic to the wrong product, or to no product at all.
    #[cfg(feature = "openhuman")]
    #[test]
    fn stays_in_sync_with_the_embedded_core_and_diverges_from_its_default() {
        assert_eq!(
            PRODUCT_IDENTITY_HEADER,
            openhuman_core::api::PRODUCT_IDENTITY_HEADER,
            "this crate's header name must match the embedded core's exactly"
        );
        assert_ne!(
            PRODUCT_IDENTITY,
            openhuman_core::api::DEFAULT_PRODUCT_IDENTITY,
            "opencompany's identity must not silently become the openhuman default"
        );
    }

    /// `feedback::tinyhumans::PRODUCT` used to carry its own `"opencompany"`
    /// literal (AC #2 of issue #376: the value must be set once, not
    /// duplicated per call site). Pinning the equality here means a future
    /// edit that re-introduces a second literal there fails this test instead
    /// of silently reintroducing the duplication.
    #[test]
    fn feedback_product_const_stays_wired_to_this_single_source_of_truth() {
        assert_eq!(crate::feedback::tinyhumans::PRODUCT, PRODUCT_IDENTITY);
    }
}
