//! The loadable TinyMemory module as a memory driver (issue #1524).
//!
//! The module is a compiled `cdylib` speaking the tinybus module ABI: admitted
//! through tinybus's descriptor, manifest and digest gates, attached to a
//! private in-process broker, and called over that bus like any other service.
//! This directory holds the host side of that arrangement — the broker
//! singleton ([`host`]), and in later commits the served callback objects and
//! the [`MemoryProvider`](tinymemory_api::provider::MemoryProvider) that
//! proxies the mandatory families across the bus.
//!
//! Everything here is dead code until the `tinymemory-module` feature is on
//! AND the `module` driver id is selected; the feature is default-off and no
//! existing configuration reaches it.

pub mod callbacks;
pub mod host;
pub mod ops;
pub mod preflight;
pub mod provider;
