//! The TinyMemory `MemoryProvider` seam (issue #914).
//!
//! One engine-neutral driver contract behind the three memory ports, with the
//! engine chosen by configuration: the embedded engine in-pod, a hosted service
//! behind a URL and a credential, or nothing at all.
//!
//! # The decorator is the whole point
//!
//! [`BoundMemory`] is the **only** public way to obtain a memory port from a
//! provider. That is a deliberate constraint, not an ergonomic accident.
//!
//! The three ports take `&CompanyId` as an explicit first argument — a
//! compiler-enforced tenant-isolation invariant. `MemoryProvider` takes
//! `namespace: &str`. Handing the raw provider to call sites would trade a
//! guarantee the compiler checks for a convention the reviewer checks, and a
//! missing prefix would be a silent cross-tenant leak with nothing to catch it.
//! With a hosted engine it is worse still: the namespace string is the only
//! thing separating tenants inside somebody else's database.
//!
//! So: [`Namespace`](namespace::Namespace) has no public constructor, every
//! port method takes `&CompanyId` and derives its namespace fresh from it
//! (`Namespace::company_root` is the only way to make one), and there is no
//! `pub fn` in this module tree that accepts a namespace string. Note the
//! enforcement lives in the *port signatures and the namespace type*, not in
//! `bind` — `BoundMemory::bind(provider, class)` itself takes no company,
//! because one bound engine serves every company this host runs.
//!
//! # What else the decorator owns
//!
//! The contract deliberately owns no policy, which leaves five duties here:
//!
//! - **Scratch firewall.** Provisional working-out lives in its own namespace
//!   and is unreachable from durable recall *by construction* — the durable
//!   facades scope recall to their own namespace and re-check every hit that
//!   comes back, so scratch cannot appear in a durable result even if a driver
//!   ignores the filter.
//! - **Archive on evict.** The contract has no archive tier, so eviction is a
//!   move between namespaces rather than a delete. See
//!   [`facades::ProviderMemoryStore::evict`].
//! - **Taint.** Inbound-channel writes are stamped
//!   [`MemoryTaint::ExternalSync`] via [`BoundMemory::inbound_context`].
//!   Note the contract's `MemoryCore::store` requires taint on every call and
//!   has no dropping default — the defaulted `store_with_taint` lives on the
//!   *engine* trait, which is exactly why nothing here wraps a bare `Memory`.
//! - **Per-agent and per-desk scoping**, which neither cognition port has today.
//! - **Operator rights** — inspect, delete, redact, export — from
//!   `docs/spec/company-brain/memory.md`.
//!
//! # Class is host-side
//!
//! [`DriverClass`] is taken from *configuration*, never from the driver. The
//! contract crate excludes it on purpose: a driver that self-reported its class
//! could claim to be embedded and skip the egress and trust checks that class
//! gates. [`bind`](BoundMemory::bind) therefore takes the class as an
//! argument rather than asking the provider for it.

//!
//! ## Who may run `migrate`
//!
//! [`migrate`](migrate::migrate) is deliberately a **local CLI operation**
//! (`opencompany memory migrate`), not an HTTP surface: it never binds a
//! route, so the only principal who can reach it is whoever already runs the
//! binary on this host and supplies BOTH engines' credentials. That person
//! owns the data on both ends by definition — an in-app authorization layer
//! here would gate the operator against themselves. If a remote-triggered
//! migration surface is ever added, it must carry its own operator-auth and
//! per-tenant scoping; do not lift this function onto a route as-is.

pub mod driver;
pub mod facades;
pub mod migrate;
#[cfg(feature = "tinymemory-module")]
pub mod module;
mod namespace;

use std::sync::Arc;

use tinymemory::registry::DriverClass;
use tinymemory_api::capabilities::Capabilities;
use tinymemory_api::provider::{MemoryProvider, audit_provider};
use tinymemory_api::types::MemoryTaint;

use facades::{Bound, ProviderContextStore, ProviderFactStore, ProviderMemoryStore};
use namespace::Scope;

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::{CompanyId, ContextStore, FactStore, MemoryStore};

pub use driver::{MemoryDriverConfig, MemoryDriverError, MemoryMode, open_driver};

/// A bound memory engine, and the only way to get a memory port out of one.
///
/// Process-scoped, like the `MemoryOverlay` it is opened into: one engine serves
/// every company this host runs, and each port method derives its namespace from
/// the `&CompanyId` it is given. See `facades::Bound` for why the company is a
/// per-call argument rather than a field — briefly, a namespace fixed at
/// construction would be one tenant's namespace serving all of them.
///
/// Clone is cheap: the provider is shared.
#[derive(Clone)]
pub struct BoundMemory {
    provider: Arc<dyn MemoryProvider>,
    class: DriverClass,
    driver_id: String,
    capabilities: Capabilities,
}

impl std::fmt::Debug for BoundMemory {
    /// Renders the driver identity and class only.
    ///
    /// Never anything from the provider's own configuration, which is where the
    /// endpoint and the credential live.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundMemory")
            .field("driver_id", &self.driver_id)
            .field("class", &self.class.as_str())
            .finish_non_exhaustive()
    }
}

impl BoundMemory {
    /// Binds `provider` as this host's memory engine.
    ///
    /// `class` comes from the host's configuration, never from the driver — see
    /// the module docs.
    ///
    /// Runs [`audit_provider`] before returning anything usable. An engine that
    /// advertises a capability family it cannot actually serve fails here, at
    /// boot, rather than on the first call that needs it — which for a memory
    /// family could be days later, on a path nobody is watching.
    pub fn bind(provider: Arc<dyn MemoryProvider>, class: DriverClass) -> Result<Self> {
        audit_provider(provider.as_ref()).map_err(|audit| {
            OpenCompanyError::Config(format!(
                "memory driver `{}` failed its capability audit at bind time: {audit}. \
                 This is an engine bug, not a configuration problem — the driver claims a \
                 capability family it cannot serve, or serves one it does not advertise.",
                provider.driver_id()
            ))
        })?;
        Ok(Self {
            driver_id: provider.driver_id().to_string(),
            capabilities: provider.capabilities(),
            provider,
            class,
        })
    }

    /// The bound engine's own name (`tinycortex`, `supermemory`, `null`, …).
    ///
    /// Safe to surface to an operator — unlike the endpoint and the credential,
    /// which are not.
    pub fn driver_id(&self) -> &str {
        &self.driver_id
    }

    /// How this driver was bound, from configuration.
    pub fn class(&self) -> DriverClass {
        self.class
    }

    /// The negotiated capability families, as stable names for status output.
    ///
    /// An operator looking at a hosted engine needs to see what it *cannot* do:
    /// most hosted services have no summary tree, no graph, and no taint, and
    /// finding that out from a failed cycle is worse than reading it here.
    pub fn capability_names(&self) -> Vec<&'static str> {
        self.capabilities.iter().map(|cap| cap.as_str()).collect()
    }

    /// Builds a facade addressing one scope of a company's memory.
    fn bound(&self, scope: Scope, taint: MemoryTaint) -> Bound {
        Bound::new(self.provider.clone(), scope, taint)
    }

    /// The operator's hand-curated facts.
    ///
    /// Operator-authored, so `Internal` — this is the company writing about
    /// itself, not content arriving from outside.
    pub fn facts(&self) -> Arc<dyn FactStore> {
        Arc::new(ProviderFactStore::new(
            self.bound(Scope::Facts, MemoryTaint::Internal),
        ))
    }

    /// The durable context store: the RLM environment the brain queries.
    pub fn context(&self) -> Arc<dyn ContextStore> {
        Arc::new(ProviderContextStore::new(
            self.bound(Scope::Context, MemoryTaint::Internal),
        ))
    }

    /// The durable context store, for writes arriving from an inbound channel.
    ///
    /// Identical to [`context`](Self::context) except that every write is
    /// stamped [`MemoryTaint::ExternalSync`]. A company that reads the web needs
    /// this: content that arrived from outside must stay marked as such, because
    /// laundering it into internal-trust content is what lets a page the agent
    /// read be treated as something the company decided.
    pub fn inbound_context(&self) -> Arc<dyn ContextStore> {
        Arc::new(ProviderContextStore::new(
            self.bound(Scope::Context, MemoryTaint::ExternalSync),
        ))
    }

    /// One agent's private partition.
    pub fn agent_context(&self, agent_id: &str) -> Arc<dyn ContextStore> {
        Arc::new(ProviderContextStore::new(self.bound(
            Scope::Agent(agent_id.to_string()),
            MemoryTaint::Internal,
        )))
    }

    /// One desk's shared partition.
    pub fn desk_context(&self, desk_id: &str) -> Arc<dyn ContextStore> {
        Arc::new(ProviderContextStore::new(
            self.bound(Scope::Desk(desk_id.to_string()), MemoryTaint::Internal),
        ))
    }

    /// Provisional working-out, unreachable from durable recall.
    ///
    /// Nothing written here can be returned by [`context`](Self::context),
    /// [`agent_context`](Self::agent_context) or
    /// [`desk_context`](Self::desk_context): those facades scope recall to their
    /// own namespace and drop any hit reported outside it, and the scratch
    /// namespace is a sibling of all three. The roles that judge get neither
    /// half — unsettled working-out read as progress is what keeps a loop
    /// retrying.
    pub fn scratch(&self) -> Arc<dyn ContextStore> {
        Arc::new(ProviderContextStore::new(
            self.bound(Scope::Scratch, MemoryTaint::Internal),
        ))
    }

    /// The brain's traces and task results.
    pub fn memory(&self) -> Arc<dyn MemoryStore> {
        Arc::new(self.trace_store())
    }

    /// The same store, concretely, for the operator rights the port does not
    /// carry.
    fn trace_store(&self) -> ProviderMemoryStore {
        ProviderMemoryStore::new(
            self.bound(Scope::Traces, MemoryTaint::Internal),
            self.bound(Scope::Archive, MemoryTaint::Internal),
            self.bound(Scope::TaskResults, MemoryTaint::Internal),
        )
    }

    /// Traces that eviction archived.
    ///
    /// An operator right, not a port method: `MemoryStore` has no way to ask
    /// this, and `evict` promises the traces still exist, so something has to be
    /// able to show them. Also what makes "archives rather than destroys"
    /// testable as a property rather than as a count.
    pub async fn archived_traces(
        &self,
        company: &CompanyId,
    ) -> Result<Vec<crate::ports::CompressedTrace>> {
        self.trace_store().archived_traces(company).await
    }
}

#[cfg(test)]
mod test;
#[cfg(test)]
mod upstream_conformance_test;
