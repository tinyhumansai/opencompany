# Deep trace

The unredacted companion of a turn's steps: model reasoning, raw tool arguments,
raw tool output. It is what makes a run *readable* rather than merely auditable.

## Why it is a separate store

A [`TurnStep`](ports-cognition.md) is what an operator sees: a label, redacted
arguments, and a *shape* of what came back ("12 items"). That is the right
disclosure for a timeline and an approval card, and it is deliberately lossy —
the raw arguments and output are dropped, and a `thinking` step keeps no text at
all.

`DeepTraceStore` (`src/ports/deep_trace.rs`) keeps the other half as a **sibling**
record rather than by widening `TurnStep`. Three things follow, and each is the
reason:

- `TurnStep`'s wire contract is untouched, so nothing that renders a timeline can
  begin leaking secrets by accident.
- `GET {scope}/runs/{id}` physically cannot disclose it: that route never calls
  this port.
- It can be purged wholesale without touching run history — the skeleton is the
  contract, the body is the luxury.

## It holds secrets, by design

Unredacted arguments include credentials passed on a command line; raw output
includes the contents of any file an agent read. Two rules, both enforced in code
rather than documented:

- The read path is company-scoped exactly as every other per-company port is.
  `assert_deep_trace_store` in `src/store/conformance.rs` pins that a run id
  shared across two companies does not leak either way.
- `runtime::approval_display::redact` is **unchanged** on the approval path.
  Widening the trace must never widen the operator-facing cards.

Note what redaction does and does not promise, because the split here depends on
it: it masks by **key name**. `approval_display`'s own module doc says "an
unlisted key holding a secret is not" safe. Raw *output* is dropped from a step
unconditionally; an argument under an unrecognised key was always visible on the
step. The deep store changes neither — it adds a second, explicitly-unredacted
surface beside them.

## Bounds

Raw output is unbounded by nature (a `cat` of a large file, a full test log), so
the caps live on the port and the prune is part of the write:

| Cap | Value |
| --- | --- |
| `DEEP_REASONING_CHAR_CAP` | 64 KiB |
| `DEEP_OUTPUT_CHAR_CAP` | 64 KiB |
| `DEEP_ARGUMENTS_CHAR_CAP` | 32 KiB |
| `MAX_DEEP_STEPS_PER_RUN` | 500 — matches the run trace's own ceiling, so the two truncate at the same ordinal |
| `MAX_DEEP_RUNS_PER_COMPANY` | 50 |

Clip, never refuse: a body too large to keep is worth keeping the head of, and
failing the write would lose the whole step. Clipping is on a character boundary,
so a multi-byte body is never split into invalid UTF-8.

**The prune drops whole runs, not the oldest rows.** Ranking rows would leave a
surviving run holding a torn half of its own trace, which reads as "the agent
stopped thinking here" rather than "this run's bodies were pruned".

`purge_deep_trace(company, run_id)` destroys bodies — one run's, or the whole
company's — and leaves every `RunStepRecord` intact.

## Storage

| Backend | Where |
| --- | --- |
| fs | `deep-trace.jsonl`, a sibling of `run-steps.jsonl`; last-write-wins per `(run_id, step_seq)` |
| sqlite | `run_step_details`, keyed `(company_id, run_id, step_seq)`, plus a recency index |
| mongodb | `run_step_details`, same key as a unique index, plus a recency index |

## How a step's two halves stay aligned

`StepTrace::push` returns the ordinal, the scrubbed step, and the unredacted
companion **from one call**. The alternative — a second pass over the same
events — would be a second state machine that has to agree with the first about
ordinals and about where a thinking run starts and ends; when it eventually
disagreed, a detail would be filed against the wrong step.

A thinking run is many `ThinkingDelta` events folding to one step, so its text
accumulates and is re-emitted under the same ordinal. The store replaces on
`(run_id, step_seq)`, so the row converges rather than stacking. It flushes every
2 KiB and again when the run closes: per-delta would be one store write per
token, and close-only would lose the whole thought if the host died mid-run.

A tool call closing a thinking run emits **two** records — the run's finalized
reasoning, then the tool's step. Dropping that tail would lose the reasoning
immediately preceding a tool call, which is the part worth reading.
