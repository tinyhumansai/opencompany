# Brain Module

The brain module holds the offline `Brain` implementation, `EchoBrain`: a
single-pass cognition seam that turns an `OperatorMessage` into one channel
response, with no network and no vendored-crate dependency. It keeps the whole
kernel pipeline testable on the default build.

`EchoBrain` is the Phase-1 stand-in for the hosted `HostedMedullaBrain` and the
TinyAgents-backed `StubBrain` (feature `tiny`); see
[`docs/spec/runtime/ports.md`](../../spec/runtime/ports.md) and
[`docs/spec/integrations/medulla.md`](../../spec/integrations/medulla.md).
Anything that needs the model backend must not land here — this module stays
dependency-light so `cargo test` runs offline.

## `medulla` submodule (the hosted `/orchestration/v1` wire)

`brain::medulla` holds the hosted-Medulla contract, all in the default build
with no network dependency:

- `medulla::wire` — typed serde for the protocol-1 envelope, the nine `ORCH_*`
  error codes, `POST /events` (`cycleId` = `cyc:<counterpart>:<session>:<seq>`,
  idempotent on `(user, counterpart, session, seq)`), `POST /world-diff`, the
  read-surface views, and the Socket.IO frames (`orch:register_tools`,
  `orch:effect:<kind>` with deterministic `callId` = `{cycleId}:{kind}:{index}`,
  `orch:effect:result`, `orch:tool_call`, `orch:tool_result`).
  `wire::assert_no_model` rejects any request body carrying a `model` field.
- `medulla::transport` — the `MedullaTransport` seam abstracting the HTTP posts,
  the per-cycle effect/tool-call stream, and the acks/answers, so the brain
  never depends on a concrete network client.
- `medulla::mock` — an in-memory `MockTransport` that scripts cycle frames and
  records brain calls, driving the seam offline in tests.

The networked `HttpSocketTransport` and `HostedMedullaBrain` itself land in a
later batch behind an optional feature; nothing in this submodule pulls a
network crate.
