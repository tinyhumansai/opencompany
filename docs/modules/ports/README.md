# Ports Module

The ports module holds the kernel's seams: one trait per file (`Brain`,
`CompanyStore`, `EventLog`, `MemoryStore`, `ContextStore`, `ChannelAdapter`,
`ToolProvider`, `AgentEconomy`, `ApprovalGate`, `SecretStore`) plus the shared
id, event, effect, and cycle payload types in `types.rs`. Trait and method
names are binding against [`docs/spec/runtime/ports.md`](../../spec/runtime/ports.md);
payload field lists may still evolve.

Traits are `#[async_trait]` so they stay object-safe behind `Arc<dyn _>`. Keep
this module free of behavior — implementations live in `store`, `policy`,
`brain`, and `runtime`.
