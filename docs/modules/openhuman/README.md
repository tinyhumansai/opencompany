# OpenHuman Module

OpenHuman is the tenant harness, embedded as a **library**. The
`src/harness/` module links `openhuman_core` (`vendor/openhuman`) directly and,
under `feature = "openhuman"`, builds one openhuman `Agent` per manifest
`[[agent]]` through `AgentBuilder`. The default build links none of it and
keeps its offline, echo-brained behaviour.

The builder seams are wired to OpenCompany's own ports:

- **Memory** → `harness::memory::OcMemory`, an openhuman `Memory` over the
  OpenCompany `ContextStore`.
- **Inference provider** → the hosted Medulla `Provider` (`harness::provider`),
  with a `MockProvider` for offline tests.
- **Approval policy** → `harness::policy::ApprovalPolicy` maps `[policy].mode`
  onto openhuman's `ToolPolicy`; the security-tier words
  (readonly/supervised/full) line up 1:1.
- **Tools / skills** → injected from the company's manifest grants.

See [`docs/modules/runtime/README.md`](../runtime/README.md) for `HarnessPool`
and [`docs/spec/integrations/openhuman.md`](../../spec/integrations/openhuman.md)
for the full integration contract.

## Cost metering (partial — pending openhuman#4940)

`harness::cost` maps a completed turn's `TurnCost` onto the ledger and the
`UsageMeter`. The mapping is complete and tested, but openhuman exposes turn
usage only through a `pub(crate)` accessor, so until the upstream public
turn-usage accessor (tinyhumansai/openhuman#4940) lands, `HarnessPool::run`
records a **zero-usage** turn (which, per the cost contract, writes nothing).
That PR is the seam for real inference-cost metering.

## `src/openhuman/` — legacy JSON-RPC path (behind `openhuman-rpc`)

The former out-of-process seam is retained for one release and then removed.
`src/openhuman/` still hosts the launcher (`opencompany open-human --dry-run`
shells out through Cargo to the vendored checkout) and the JSON-RPC adapters —
`rpc.rs` (the `OpenHumanRpc` transport trait + `MockOpenHumanRpc`),
`http_client.rs` (the `reqwest` client behind `openhuman-rpc`), `tools.rs`
(`OpenHumanToolProvider`, catalog filtered by manifest grants, ungranted calls
rejected), and `channel.rs` (`OpenHumanChannelAdapter`). It degrades to
built-in tools and the operator channel with a boot warning when OpenHuman is
unreachable — never a boot failure. New work targets the embedded library, not
this path.
