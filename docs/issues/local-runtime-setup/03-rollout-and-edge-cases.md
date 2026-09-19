# Rollout, edge cases, blast radius

## Order

Four slices. Each is independently shippable and independently revertible; none
depends on a later one.

**Slice 1 — tell the truth on a failed draft probe.** Host only. Swap
`probe::describe` for the not-connected wording in `probe_draft_inner`
(`providers.rs:2554`), and carry host **and port** into the subject. Ship this
alone. It is a string change with a test, it fixes the only factually wrong
sentence in the flow, and it needs no console change to be worth having.

**Slice 2 — `Test connection` on the endpoint step.** Console only. Reuses
slice 1's message, so it lands after it. Adds a button and a result line; no
new route, no new state on the host.

**Slice 3 — hold the endpoint step on an `endpoint`-class failure.** Console
only, and the one with real behaviour change: it stops the flow advancing into
a model step that cannot help. Do it after slice 2, because once Test exists
the held step has something to offer besides a dead end.

**Slice 4 — defaults and copy.** LM Studio's `defaultEndpoint`, the local
endpoint placeholder, and `helperLocal`. Touches both catalogue tables
together. Independent of 1–3; sequence it wherever it is convenient.

Slices 1 and 4 are small enough to be one PR each. Slices 2 and 3 are arguably
one PR, since 3 is what makes 2 worth having on the failure path.

## Edge cases

**A runtime that answers but publishes nothing.** Ollama with no models pulled
returns `{"object":"list","data":null}` — `data` is `null`, not `[]`. Observed
directly on a fresh install. The probe treats this as a successful read of an
empty catalogue, so the model step opens on free text with no error, which is
the *correct* outcome and a different state from a failed probe. Whatever
slice 3 does must not conflate "reachable, nothing installed" with "nothing
answered" — the first wants "reachable, but no models are installed; pull one",
the second wants the gap-1 sentence. **This is the state most likely to be got
wrong**, because both end up on free text.

**Test pressed, then the endpoint edited.** A stale "Reachable" line under a
changed address is a lie of the same family as gap 1. Clear the result on any
change to the field.

**Test pressed twice, or Test then Continue.** `ProvidersTab.tsx` already
guards concurrent probes with its `attempt` counter, and the dialog already
refuses Escape and overlay clicks while `busy`. Route Test through the same
guard rather than adding a second one.

**An operator who does not press Test.** Must stay fully supported. Continue's
existing behaviour is the fallback, and `Add anyway` remains the escape for a
provider whose probe fails but which the operator knows is right.

**Azure and `freeTextOnly`.** Untouched by all four slices. An Azure endpoint
routes on a deployment name its `/models` never publishes, so it defaults to
text by design; nothing here should make that path narrower.

**Hosted tenants.** Slice 4's copy change is the minimum. Whether the local
group should be disabled outright on a hosted deployment is a product decision
this brief does not make — but leaving the copy claiming "this machine" is not
an option, because it is false there and it costs the operator a debugging
session to discover.

**The catalogue parity test.** Console and Rust catalogues are mirrored and a
test asserts they agree. Slice 4 must move both; changing one is a red build.

## Blast radius

| Change | Reaches |
|---|---|
| Slice 1 | The draft-probe route only. Both other `probe::describe` call sites (`providers.rs:747`, `:2461`) are post-write and must keep the "Saved" wording — **do not change the function, change the caller.** |
| Slice 2 | The add/edit dialog. No route, no DTO, no stored state. |
| Slice 3 | The add flow's step transition for one failure class. |
| Slice 4 | Two catalogue tables and one copy constant. |

The thing to be careful about is in slice 1: `probe::describe` is shared with
paths where "Saved, but…" is the true sentence. Editing the shared function to
fix the draft probe would put a false sentence on the rollback paths instead —
the same bug, moved. Fix the call site.

## What to test

**Slice 1.** A unit test asserting the draft probe's `endpoint`-class message
does not contain "Saved", and does contain the port. The second half matters:
the port regression is invisible to a test that only greps for "Saved".

**Slice 2 and 3.** The console suite already covers the add flow; extend it for
a probe that fails with `class: "endpoint"` and assert the step does not
advance. A test that only asserts the button renders proves nothing about the
behaviour this brief is about.

**Slice 4.** The existing catalogue parity test covers the table change. Add
nothing for the copy.

**End to end, and this is not optional.** All of it should be exercised against
a real local runtime, not a stub. The stub path is what made the original
symptom look like a missing dropdown for as long as it did; ten minutes of
`ollama serve` settles questions that reading settles wrongly. The full recipe
is in [`01-what-already-works.md`](01-what-already-works.md).

## One item flagged, not asserted

The exact wording for slice 1 is left open on purpose. `describe_refusal`'s
existing sentence — *"Nothing answered at {subject}, so it was not connected.
Start it and try again."* — is accurate on the draft path, but it was written
for a rollback after an attempted add, and "it was not connected" reads
marginally differently before the operator has tried to connect anything.

Whoever implements slice 1 should read both call sites in context and decide
between reusing `describe_refusal` and adding a third arm to that table. This
brief asserts the current string is **wrong**; it does not assert which of the
two replacements is **right**.

## Not in scope

Everything in `02`'s closing section: no second model picker, no change to
endpoint normalization, no background reachability poller, no change to the
SSRF loopback allowance. Each of those was considered and rejected with a
reason recorded there.
