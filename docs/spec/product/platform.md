# Platform Operators

Two consumption modes, one invariant: storage and surfaces belong to the
platform; the kernel and its guarantees come from the crate.

## Embed mode

Depend on the `opencompany` crate and construct companies programmatically:

```rust
let runtime = RuntimeBuilder::new(manifest)
    .company_store(my_postgres_store)   // any impl of the port traits
    .event_log(my_postgres_log)
    .memory_store(my_s3_memory)
    .context_store(my_vector_store)
    .build()?;
```

- **Bring your own persistence.** The four storage ports
  ([runtime/ports.md](../runtime/ports.md)) are the entire storage contract;
  DB-agnosticism means the kernel never names an engine. A conformance test
  suite (Phase 5) validates operator-supplied stores: isolation by
  `CompanyId`, append-only event/ledger semantics, export totality.
- **Bring your own surfaces.** The platform builds its own UI against the
  operator API or embeds `CompanyRuntime` directly and skips HTTP.

## Hosted multi-tenant mode

Run `opencompany serve` with a `CompanyRegistry` of many companies:

- **Provision** via `POST /api/v1/companies` with a manifest; per-company
  lifecycle controls (`pause`, suspend, archive) —
  [runtime/api.md](../runtime/api.md).
- **Auth**: platform-issued JWTs per tenant; platform-scope claims for
  provisioning and suspension. The prosumer-style operator token never
  crosses tenants.
- **Webhooks** instead of polling: `approval.requested`, `work.completed`,
  `feedback.created`, `budget.exhausted`, delivered at-least-once with
  signature headers.
- **Ledger read** per company for billing pass-through
  (`GET /api/v1/companies/{id}` includes budget burn; the ledger itself
  exports with the bundle).

## Tenancy and isolation (normative)

- Every storage port call carries `CompanyId`; stores MUST enforce isolation
  at that boundary — shared-table implementations must prove it in the
  conformance suite.
- Secrets are per-company (`SecretStore`); tool grants never cross
  companies.
- One hosted-brain session per company
  ([integrations/medulla.md](../integrations/medulla.md)); the proposed v2
  backend adds first-class multi-company multiplexing.
- Budgets are per-company hard caps; a tenant exhausting its budget pauses
  only itself.

## Operational concerns

- **Quotas**: max companies per tenant, per-company event-rate limits —
  enforced at the API layer, configurable by the platform.
- **Upgrades/migration**: export/import is total by construction
  ([runtime/lifecycle.md](../runtime/lifecycle.md)), so moving a company
  between hosts, stores, or versions is tar-out/tar-in; the manifest schema
  is versioned with compatibility guarantees
  ([templates.md](templates.md)).
- **Observability**: tracing via the TinyAgents Langfuse exporter (proxied
  through the TinyHumans backend), plus the SSE event stream per company;
  platforms attach their own log/metrics stack around the process.
- **Who holds the TinyHumans credential**: the platform, one per deployment
  or per tenant — both work; billing granularity follows the credential.
