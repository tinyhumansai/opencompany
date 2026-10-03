# `store/memory` — the TinyMemory v2 engine seam

Binds the three memory ports (`MemoryStore`, `ContextStore`, `FactStore`) to one
TinyMemory v2 `MemoryEngine` behind a tenant boundary. Compiled only with the
`tinymemory` feature. Operator-facing behaviour, env vars and the switch runbook:
`docs/spec/runtime/memory-engine.md`.

| File | Responsibility |
| --- | --- |
| `mod.rs` | Module header and `BoundMemory`: the only public way to get a port out of an engine; scoped (agent/desk) contexts, archived-trace access |
| `namespace.rs` | `Namespace` and `Scope`: the company-derived `oc/<company>-<hash>/<scope>` workspace, with no public constructor |
| `facades.rs` | Module header, record-to-item mapping, the context and fact facades |
| `facades/bound.rs` | `Bound` (one scope over one engine), the JSON envelope, namespace re-check, `EXTERNAL_TAG` provenance stamping |
| `facades/traces.rs` | `ProviderMemoryStore`: traces, task results, archive-on-evict |
| `driver.rs` | `OPENCOMPANY_MEMORY*` to a built engine: `open_driver`, `canonical_engine_id` (the `cortex` alias and the retired-engine refusals) |
| `null.rs` | `NullEngine`, the host-side "no memory" engine behind `OPENCOMPANY_MEMORY=null` |
| `migrate.rs` | `opencompany memory migrate`: `list` then `store` across two engines, idempotent by fingerprint; `resolve_migrate_configs` holds every refusal |

Tests sit beside each file as `<stem>_tests.rs` (`driver_tests.rs`,
`migrate_tests.rs`, `null_tests.rs`, `namespace_tests.rs`, `facades_tests.rs`),
plus `memory_tests.rs` and `memory_behavior_tests.rs` for the bound-memory
behaviour as a whole.
