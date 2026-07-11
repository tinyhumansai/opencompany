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
