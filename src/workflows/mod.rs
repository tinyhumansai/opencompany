//! Run a company's workflows on the **tinyflows** engine (issue #29, epic #26).
//!
//! A company's workflow graphs live on disk as
//! [`WorkflowFile`](crate::company::workflow_file::WorkflowFile)s — a data-only
//! node/edge model with six node kinds (trigger / agent / tool_call /
//! http_request / condition / output). This module runs one directly on the
//! embedded [`tinyflows`] engine, with **agent** nodes routed to the company's
//! [`HarnessPool`](crate::harness::HarnessPool) so a step inherits the roster's
//! persona / model / memory / approval policy / metering (never a second pool).
//!
//! Compiled only under `feature = "openhuman"`. tinyflows is host-agnostic and
//! `Config`-free by design, so nothing here boots an OpenHuman global `Config`,
//! registry, or backend-proxied model — the shared architecture rule for this
//! epic. The default build links none of it.

/// Compile-only proof (P0) that the vendored `tinyflows` engine links under the
/// `openhuman` feature and its public API is reachable from this crate.
///
/// Naming a `tinyflows` public item here forces the dependency to resolve and
/// the version to align at build time; it is exercised by
/// [`tinyflows_engine_is_linked`](tests) so it is not dead code.
pub fn tinyflows_engine_name() -> &'static str {
    tinyflows::product_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tinyflows engine is linked and its API answers — the P0 link proof.
    #[test]
    fn tinyflows_engine_is_linked() {
        assert_eq!(tinyflows_engine_name(), "tinyflows");
    }
}
