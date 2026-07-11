# OpenHuman

OpenHuman (vendored at `vendor/openhuman`, written against v0.58.x) is the
local-first personal-AI product: a Rust core with a Tauri/React shell. For
OpenCompany it is the **preferred backend for tools, channels, credentials,
and policy** — roughly 60 mature domains (memory, threads, channels,
subconscious, routing, providers, tools, skills, cron, workflows, wallet,
security, people, embeddings, …) that the kernel should consume, not copy.

## Integration seams

- **Process**: the existing launcher (`opencompany open-human [--dry-run]`)
  shells out through Cargo to `openhuman-core` (Core) or the Tauri app
  (Desktop); alternatively `OPENCOMPANY_OPENHUMAN_URL` attaches to an
  already-running `openhuman-core serve`.
- **Wire**: JSON-RPC at `http://127.0.0.1:<port>/rpc` (methods
  `openhuman.<namespace>_<function>`, per-launch bearer) plus REST
  `GET /health`, `GET /schema`, `GET /events`.
- **Ports backed by OpenHuman**:
  - `ToolProvider` → the tools/skills domains (`OpenHumanToolProvider`),
    catalog filtered by the company's manifest grants.
  - `ChannelAdapter` → the channels domain (email and the other messaging
    surfaces) as `OpenHumanChannelAdapter`s.
  - `ApprovalGate` → mapped onto OpenHuman's security tiers
    (readonly/supervised/full), which is why the manifest `[policy].mode`
    uses the same three words.
  - `SecretStore` (optional) → the credentials domain.

All of these degrade: OpenHuman unreachable means built-in tools and the
operator channel only, with a boot warning
([runtime/config.md](../runtime/config.md)).

## Desktop story

The Tauri app is a natural prosumer install path: OpenHuman as the shell,
OpenCompany as the company runtime behind it. Whether the prosumer UI ships
as an OpenHuman mode or a separate frontend is an open product question
([product/prosumer.md](../product/prosumer.md)); the runtime API is the same
either way.

## Upstreaming policy

Glue that adapts OpenHuman's RPC to kernel ports lives in OpenCompany.
Anything that changes OpenHuman behavior goes upstream. Candidate PRs
identified so far:

1. **Headless multi-workspace mode** — `openhuman-core serve` today serves
   one local persona; a `--workspace <id>` scope (or workspace param on RPC
   methods) would let one daemon serve N companies.
2. **Library-crate split** — expose the tool/channel/credential/policy
   domains as an embeddable crate so co-located hosts can link instead of
   RPC.
3. **External approval hook** — policy tiers currently resolve in-app; a
   webhook/RPC callback would let OpenCompany's `ApprovalGate` be the
   resolver of record.
4. **Namespaced credentials** — per-workspace credential scoping so company
   A's secrets are invisible to company B.
5. **Documented `/events` schema** — the REST event stream exists but has no
   stable documented schema for external consumers.
6. **Brain-protocol port** — make OpenHuman's own orchestration loop
   pluggable so an OpenHuman instance could delegate cognition to a hosted
   Medulla brain (the inverse of our integration).
