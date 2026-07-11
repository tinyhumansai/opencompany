# Runtime Module

The runtime module assembles the kernel and drives it. `CompanyRuntime` is the
port bundle from [`docs/spec/runtime/ports.md`](../../spec/runtime/ports.md)
(brain, stores, tools, channels, approvals), built by `RuntimeBuilder` with
file-based defaults. `CycleRunner` implements the serial-per-company cycle from
[`docs/spec/runtime/lifecycle.md`](../../spec/runtime/lifecycle.md):
drain → load → think (`Brain::run_cycle`) → gate (`ApprovalGate`) → persist.

Effects are journaled before execution and marked after, so replay never
re-fires a completed effect (at-most-once). `CompanyRegistry` maps `CompanyId`
to a running runtime, serving both the single-company and multi-tenant cases
with one type. Approval resolution schedules a follow-up cycle so the brain
learns the verdict.
