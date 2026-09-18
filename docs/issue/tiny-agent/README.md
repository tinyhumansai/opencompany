# Tiny: a built-in system-health DM

Design doc for [issue #2375](https://github.com/tinyhumansai/opencompany/issues/2375):
a permanent, non-removable DM where the host itself tells the operator about
things that need attention — a connection failing, budget running low, a
workflow failing repeatedly, an approval backlog — and later, with approval,
can act on some of them.

This is a **design doc, not an implementation**. No runtime code changes ship
with it. A follow-up issue scopes Phase 1 work items once this is reviewed.

## Reading order

1. [`design.md`](design.md) — the core decision (identity separate from
   cognition), the architecture, the phased rollout, and what each signal
   (connections, credits, usage) actually costs to wire up, each claim
   anchored to the real source it's based on.
2. [`edge-cases.md`](edge-cases.md) — edge cases to design against up front,
   and the open decisions this doc deliberately leaves for a human to make
   rather than presupposing an answer.

## The one-sentence version

Tiny is a reserved name and a pinned DM slot, not a roster agent — the
health-check behavior is plain, deterministic host code triggered on a timer,
not an LLM turn, at least through Phase 1. See [`design.md`](design.md) for
why, including the two independent risks (a self-triggering budget spiral, and
a trust-asymmetry social-engineering surface) that rule out putting a model in
the loop on day one.
