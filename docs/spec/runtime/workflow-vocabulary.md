# Workflow node-kind vocabulary

Three node-kind sets sit behind a company workflow, and they are deliberately
**not** the same size. This document is the authoring contract: which node
kinds an author may write, what each one becomes when it runs, and — just as
importantly — which engine kinds OpenCompany refuses on purpose and why.

## The authoring contract is `workflow_file.rs`, not the vendored catalog

The tinyflows engine defines a catalog of node kinds in its own source
(`NODE_KINDS` in the vendored `tinyflows/src/catalog.rs`). That catalog is the
*engine's* vocabulary, not OpenCompany's. The set an author may actually write
is the narrower `WORKFLOW_NODE_KINDS` in
[`src/company/workflow_file.rs`](../../../src/company/workflow_file.rs): the
parser accepts exactly those kinds and rejects everything else at parse time,
listing the accepted set in the error. Reading the vendored catalog to learn
"what a workflow may contain" gives the wrong answer — always read
`WORKFLOW_NODE_KINDS`.

The relationship is a strict nesting:

```
builder (BUILDER_NODE_KINDS)  ⊂  parser (WORKFLOW_NODE_KINDS)  ⊂  engine (NODE_KINDS)
        4 kinds                          12 kinds                       15 kinds
```

Each layer is a superset of the one to its left. An author writes within the
parser set; the host's automatic builder emits within an even narrower set (see
[The builder tier is narrower still](#the-builder-tier-is-narrower-still)); the
engine can run more than either accepts.

## The 12 accepted kinds and what each lowers to

`translate()` in
[`src/workflows/translate.rs`](../../../src/workflows/translate.rs) maps every
accepted kind onto a tinyflows engine kind. Eleven are identity mappings — the
on-disk kind and the engine kind are the same string. The one exception is
`output`, which the engine has no kind for, so it lowers to a bare
`transform`.

| OpenCompany kind | Lowers to (tinyflows) | Note |
| --- | --- | --- |
| `trigger` | `trigger` | identity |
| `agent` | `agent` | identity; `agent_ref` routes to the company `HarnessPool` |
| `tool_call` | `tool_call` | identity; runs a real toolbelt tool, fail-closed on `[tools].allow` |
| `http_request` | `http_request` | identity; routes through the SSRF-guarded `GuardedHttpClient` |
| `condition` | `condition` | identity; edge labels map to `true`/`false` ports |
| `output` | `transform` | **not identity** — see below |
| `switch` | `switch` | identity |
| `merge` | `merge` | identity |
| `split_out` | `split_out` | identity |
| `transform` | `transform` | identity |
| `output_parser` | `output_parser` | identity |
| `sub_workflow` | `sub_workflow` | identity |

`tool_call` and `http_request` are fully wired and execute for real (see
[`src/workflows/caps/mod.rs`](../../../src/workflows/caps/mod.rs)); they are not
structural placeholders.

## `output` lowering and host-side delivery

tinyflows has no `output` kind. An `output` node lowers to a `transform` node
with no `set` config — a pure pass-through, which is exactly the terminal
"report back" semantics: its predecessors' items flow through unchanged.

An `output` node may also carry a `destination` (`owner` / `email` / `channel`,
from `WORKFLOW_DESTINATION_KINDS`). That destination is **deliberately not
translated** into the engine graph. Delivery runs host-side, after the engine
returns, in [`src/workflows/delivery.rs`](../../../src/workflows/delivery.rs);
the engine has no use for a `destination` key and it would be inert cargo in
node config. A destination-bearing `output` node therefore lowers to the same
bare pass-through `transform` as one without a destination — pinned by the
`an_output_destination_never_reaches_the_engine_graph` test in `translate.rs`.

## A node has three outcomes, and the third is the host's (issues #881, #880)

`WorkflowNodeStatus` is `ok` / `error` / **`blocked`**. The engine reports only
the first two — it knows success and failure and nothing else — so `blocked` is
a **host** reading, applied on the way out. That is the same move
`WorkflowRun.cancelled` makes for a deliberate stop: a cancelled run is not a
failed one, and neither is a blocked one.

A node blocks when a tool call **inside its agent turn** was parked for operator
approval. This is a different mechanism from an authored or policy gate on a
`tool_call` *node*, and the difference is the whole of #881:

| | gate on a `tool_call` node | gate on a call inside an agent turn |
| --- | --- | --- |
| Who sees it | the engine — the node carries `requires_approval` | nobody outside the agent: the call is refused inside the model's tool loop |
| What the run does | pauses; the node id lands on `pending_approvals` | before #881: **finished green**, with the model's prose about the blockage as the node's output |
| What approving does | resumes the lineage through [`workflow_resume`](../../../src/runtime/workflow_resume.rs) | nothing to this run — see below |

Since #881 the second column stops the branch: the capability returns an error,
which under the default `on_error = "stop"` halts the run at that node with no
retry, and the host relabels the node `blocked`, unions its id into
`pending_approvals`, and settles the run **without** an error.

**Approving does not resume a blocked run, deliberately.** An agent node is not
re-enterable: `NodeControl::Interrupt` discards the activation's state and
re-runs the node from the top, so resuming would spend a fresh inference turn,
call the same gated tool, and park a *new* card — approve, re-run, re-park,
forever. The engine says as much itself (`StopReason::Paused` maps to "resuming
a paused agent is not supported yet"). The operator decides the card and runs
the workflow again.

An author who writes `on_error = "continue"` or `"route"` on a blocked node
still gets a surviving branch — they asked for it — and the run-level record
stays truthful either way.

### What a run says it parked

`WorkflowRun.approvals` is one row per approval the run parked: `{ nodeId, tool,
outcome, approvalId }`, where `outcome` is `parked` / `parkFailed` /
`discarded`. The two failure arms are the point — before this, a park that could
not be performed was recorded only by a `tracing::error!`, which is the sole
trace that a call the operator will never be asked about was dropped.

It is named for what the run **parked**, not for what is still outstanding, and
the console's wording follows: *"parked N approvals"*, never *"waiting on N"*.
A receipt of an event cannot go stale; a settle-time count of what is still
waiting becomes a fresh lie the moment somebody approves one. The Approvals
page is the live source of truth.

Two neighbouring fields mean narrower things and are not substitutes:
`pending_approvals` is node ids (the engine's gate nodes plus the host's blocked
ones), and `deliveries` is per-`output`-node routing only. Both were truthfully
empty on the runs that prompted #880.

Every new field carries `#[serde(default)]`. `CompanyEvent::WorkflowRunFinished`
is replayed at boot, so a field without one makes every pre-existing journal
line fail to parse — silent history loss, not a compile error.

### The reserved keys a continuation's trigger input carries

Approving a gate does not resume a paused run — it starts a new one (see
[approvals](../company-brain/approvals.md)). Four keys on that run's trigger
input carry what the lineage already knows, three of them reserved and
host-only: an author never writes them, the engine never reads them as anything
but opaque trigger data, and all three are stripped before two parked gates are
compared for dedupe.

| Key | Issue | Carries |
| --- | --- | --- |
| `approvals` | #395 | the gate node ids the replay may proceed through. Since #978 this is **every** node the run's batch approved, not just the last one clicked |
| `__opencompany_delivered` | #438 | `{node, kind}` per `output` node whose report already went out, so a continuation does not re-mail it |
| `__opencompany_performed` | #846 | `{node, tool, result}` per `tool_call` node whose call already left the building, replayed instead of re-made |
| `__opencompany_denied` | #978 | gate node ids the operator refused, or that expired to a default-deny. `park_pending_gates` skips them, so a refusal is final rather than re-asked |

All four **accumulate down the lineage**: a two-gate graph unions what its own
card carried with what the run added, or the second gate forgets the first's
decisions and the third run re-sends, re-calls or re-asks.

## What an `agent` node receives from upstream, and its bound

`translate()` binds `input = "=items"` on every `agent` node, so a node's turn
carries the resolved output of **every** direct predecessor — that is what makes
a fan-in (`gather N sources → rank them`) deliver all N rather than the first.
The fold happens in
[`src/workflows/caps/mod.rs`](../../../src/workflows/caps/mod.rs), under the
heading `## Input from the previous step`.

That fold is **bounded**
([`src/workflows/caps/upstream.rs`](../../../src/workflows/caps/upstream.rs)).
The section one agent node's turn carries is never longer than
`DEFAULT_UPSTREAM_BUDGET_CHARS` (32,000) characters — **including every
truncation marker and separator**, not merely the source text inside them. The
budget is divided max-min fairly across the predecessors, so a short source is
served in full and the large ones split what is left and cannot crowd it out. A
model that advertises an input window may lower that budget for its own size; it
may never raise it, because the advertised figure describes the model rather than
the turn and says nothing about the system prompt, the tool schemas or the
teammate's session history sharing the same window.

The accounting is paid for three ways, each where it fits: separators and the
omitted-sources line are deducted up front; each truncation marker is reserved
out of the allowance of the source it describes; and predecessors past
`max_rendered_sources` — the point where each one's fair share falls below a
readable `MIN_SOURCE_SHARE_CHARS` — are aggregated into a single `[OMITTED BY
OPENCOMPANY — N of M inputs …]` line rather than rendered as unreadable shards.
That last cap is what makes per-source markers affordable at all: without it a
thousand-way fan-in (a `split_out` over a large array is enough) adds a thousand
markers, which is how the first cut of this bound could emit 243,886 characters
under a 32,000-character budget.

Three properties are contractual:

- **The bound is at the join, not at the producer.** A cap on a `tool_call`
  node's own output would bound each fetch separately and still let three
  bounded fetches sum to an oversized turn, and it would miss the other
  producers — an upstream `agent` reply and a `transform` payload are unbounded
  in exactly the same way. One rule at the join covers a single 500KB
  `web_fetch` and a three-way fan-in alike.
- **Truncation is never silent, and never unbudgeted.** Each cut source carries
  a `[TRUNCATED BY OPENCOMPANY — source i of n …]` marker in the turn, so the
  agent knows it is holding a fragment, and the run carries an operator notice
  on
  [`WorkflowRun::notices`](../../../src/ports/workflow_runner.rs) naming how much arrived, how much
  fitted, how many inputs were dropped entirely, and what to do instead. The
  markers are inside the budget they report on — a bound whose own reporting can
  breach it is not a bound.
- **A bounded turn is not a failed run.** The upstream work is already paid for
  by the time the fold happens, so an oversized input truncates and reports
  rather than failing the node and discarding that work.

A provider that refuses a turn on its context window anyway — the remaining
causes are the node's own instruction, its tool schemas, or the teammate's
accumulated session history — has its error rewritten before it reaches the run,
because the vendor wording ("the conversation is too long … please start a new
chat") describes a chat product: a workflow step has no conversation an operator
owns and no chat for them to start.

## A node's declared postcondition halts before its output flows downstream (issue #1866)

A `postcondition` is a mechanical, deterministic check on a node's OWN output,
run before the run is allowed to advance past that node — the general form of
the truncation check the iteration-cap signal (#1865) already applies, but
author-declared instead of engine-derived. It is a different mechanism from
`on_error`/`retry` (which react to the node's *call* failing) and from the
`blocked` outcome above (which reacts to a *tool call inside the turn* being
parked) — a postcondition reacts to the call succeeding with an output the
author decided is not good enough to hand to the next node.

**`agent` nodes only, in this slice** — `tool_call`/`http_request` are a
follow-up (see the issue). Declared as a table on the node, evaluated by
[`evaluate_postcondition`](../../../src/workflows/caps/postcondition.rs) inside
[`HarnessAgentRunner::run_turn`](../../../src/workflows/caps/mod.rs):

```toml
[[node]]
id = "research"
kind = "agent"
name = "Research"
agent = "analyst"

[node.postcondition]
require = "field_present"
field = "json.items"
```

`require` is one of three predicates, checked against the agent's reply
best-effort parsed as JSON (`Value::Null` when it is not valid JSON — the
common case, since agent nodes are prose by default):

| `require` | Checks | `field` |
| --- | --- | --- |
| `non_empty` | the reply's prose (`text`) is present and non-blank after trimming | unused |
| `field_present` | the dotted `field` path resolves to a present, non-null value inside the parsed reply | **required** |
| `non_empty_list` | the target is a JSON array with at least one element | optional — the whole parsed reply when omitted, else the dotted path within it |

`field` is a dot-separated path evaluated against the reply's structured
content — `json.items` in the example above means "the agent's reply, parsed
as JSON, must have a top-level `items` key" (so a reply of
`{"items": [1, 2, 3]}` satisfies both `field_present` on `json.items` and
`non_empty_list` on the same path). The `json.` prefix mirrors the engine's
`{ json, text, raw }` item envelope everywhere else in this document — see
[What an agent node receives from upstream](#what-an-agent-node-receives-from-upstream-and-its-bound)
— it does not name a real top-level `json` key on the reply itself.

**A `field` must be rooted at `json`, `text`, or `agent_ref` — nothing else
ever resolves.** The evaluator checks `field` against the exact `{ text,
agent_ref, json }` envelope shown above, not the parsed reply directly, so a
bare structured field like `field = "items"` (missing the `json.` prefix)
can never resolve — `resolve_path` looks for a top-level `items` key on the
envelope, which is never there regardless of what the agent replies.
`parse_workflow` refuses this at author time (issue #1937 boundary sweep)
with a message pointing at the `json.`-prefixed form, the same way it refuses
`field_present` with no `field` at all above — a gate that can never pass is
as much an authoring bug as one that is missing entirely.

**Only `json` has anything to descend INTO.** `text` and `agent_ref` are
always plain strings in the envelope `field` resolves against, so while
`field = "text"` or `field = "agent_ref"` are valid roots on their own, a
dotted descendant of either — `field = "text.foo"`, `field =
"agent_ref.id"` — can never resolve any more than a bare `items` could:
indexing a string with a further path segment always comes back empty.
`parse_workflow` refuses these the same way, at author time. `json` is the
only root with real structure to walk into (`json.items`, `json.result.count`,
…).

**The combination rule: `require`'s accepted value kinds ∩ `field`'s
possible value kinds must be non-empty, or the gate can never pass.** Every
rejection on this page — bare `items`, a `text.`/`agent_ref.` descendant,
`non_empty_list` on `text`/`agent_ref` — is the same structural fact, not a
list of unrelated special cases: `text` and `agent_ref` can only ever hold a
string (the envelope guarantees it), `json` (bare or dotted) can hold
anything the agent's reply parses to, and each `require` only accepts
certain kinds back from its target. When a root's guaranteed kind and a
predicate's accepted kinds don't overlap, no reply can ever satisfy the
gate, and `parse_workflow` refuses it before it can save. The full
satisfiability table, `field` (columns) against `require` (rows) — omitted
means no `field` set at all:

| `require` | omitted | `text` | `agent_ref` | `json` | `json.<path>` |
| --- | --- | --- | --- | --- | --- |
| `non_empty` | ✅ (ignores `field`) | ✅ (ignores `field`) | ✅ (ignores `field`) | ✅ (ignores `field`) | ✅ (ignores `field`) |
| `field_present` | — (`field` required at author time) | ✅ always (never null) | ✅ always (never null) | ✅ if object/array/scalar reply — but a bare scalar is then refused at EVALUATION time (below), a delivery constraint the author-time kind check can't see | ✅ if the path resolves to non-null |
| `non_empty_list` | ✅ if the reply is a bare array | ❌ never — `text` is never an array | ❌ never — `agent_ref` is never an array | ✅ if the reply is a bare array | ✅ if the path resolves to an array |

`non_empty` ignores `field` entirely (a set-but-unused `field` alongside it
is inert, not rejected — there is nothing for it to conflict with).
`field_present` accepts any non-null kind, so it never conflicts with a
root's fixed kind; the `json`-scalar cell is the one place a check beyond
kind-intersection is needed, covered next. Only `non_empty_list`'s narrow
"array only" acceptance ever collides with `text`/`agent_ref`'s fixed
"always string" kind — the two ❌ cells above.

**`field_present`'s bare-scalar-under-`json` refusal is a DIFFERENT check,
at a different time, for a different reason.** The table above is about
*author-time* kind compatibility (could a value of this KIND ever satisfy
this predicate). A bare scalar reply passes that check for `field_present`
on `json` (a scalar is a valid non-null kind) — the problem shows up only at
*evaluation* time, and only about *delivery*: the engine's own envelope
construction discards a scalar (see the emission section below), so
certifying one would pass a gate whose value never reaches a downstream
binding. That is why it is enforced in
[`evaluate_postcondition`](../../../src/workflows/caps/postcondition.rs), not
[`validate`](../../../src/company/workflow_file.rs) — it depends on the
runtime value's actual shape, not the field path's static kind.

**`field` is not the place for an `=`-expression.** `postcondition` lowers
into the same engine-resolved node config as everything else in this
document, so it is tempting to write `field = "=item.some_key"` the way
`args`/`input`/etc. do elsewhere. Don't: the rooted-`field` rule above
already refuses it at author time (no `=`-expression's first dotted segment
can ever equal `json`/`text`/`agent_ref`), and even if a graph reached
runtime with one anyway — an older validator, a hand-edited seed file — a
`field` that resolves away to null is refused at evaluation time too, rather
than silently letting the reply through: `field_present`'s whole job is
checking that one named field exists, so a resolution quirk that erases the
name it was told to check fails the gate, not the other way around.

**`json.text` and `json.agent_ref` are reserved and refused at save.** The
emitted output always carries the raw reply string under `text` and the real
roster id under `agent_ref` — that is what lets `delivery.rs::report_text`
keep finding prose in a delivered report for the overwhelming majority of
nodes whose reply isn't structured at all, rather than the literal string
`"null"`. A `field` that drills into the *parsed* reply's own `text` or
`agent_ref` key (e.g. `json.text` when the model happens to answer with
`{"text": [...]}`) would validate whatever shape the model put there, but a
downstream `=item.json.text`/`=item.json.agent_ref` binding always reads the
raw reply string / real roster id regardless — a gate that can pass on one
value while the field it named resolves to a different one downstream.
`parse_workflow` refuses this at author time, naming both reserved paths in
the error, rather than let a workflow ship a gate that certifies a shape the
emitted output can never actually hold.

**No-field `non_empty_list`** checks the parsed reply as a whole: only a reply
that IS the literal JSON text of a non-empty array (`["a", "b"]`) passes —
`non_empty_list` accepts arrays and nothing else, so a plain-prose reply, a
reply that parses to a scalar, and a reply that parses to a JSON *object*
(`{"a": 1}`) all fail with "is not a list — the shape does not match.". An
unrecognized `require` value is rejected at author time by
[`validate`](../../../src/company/workflow_file.rs) — a graph naming one never
saves — and fails OPEN (a `tracing::warn!`, the node proceeds) if it somehow
still reaches the evaluator, the same fail-open stance
[`HarnessAgentRunner::run_turn`](../../../src/workflows/caps/mod.rs)'s own
module doc applies to a failed attempt-row mint: observability must never be
able to fail the work it is observing.

**On failure, the node halts — the same shape as `on_error = "stop"`.** The
attempt settles `Failed` with a plain-English message naming the gap — e.g.
"the node's output is missing `json.items` — the expected field never
landed." — `run_turn` returns `Err`, and because `on_error` defaults to
`"stop"` and `retry.max_attempts` to `1`, nothing downstream ever sees the
insufficient output. This runs BEFORE the iteration-cap check (#1865), deliberately: a
capped turn's partial reply is exactly the truncation class a postcondition is
meant to catch, so a declared postcondition is checked regardless of whether
the cap already would have failed the attempt on its own.

**On success, the node's emitted output reflects what the gate certified —
except a bare scalar, which the gate refuses to certify in the first
place.** When (and only when) a postcondition is declared, the node's output
— what a downstream `=item.json.<field>` binding reads — is enriched with
the parsed reply: an object reply's own keys are merged in (so `json.items`
above also resolves downstream, not only inside the gate's own check), and a
bare-array reply (`["a", "b"]`) replaces the emitted value with the array
itself (so `=item.json` resolves to the exact array `non_empty_list`
validated). A bare **scalar** reply (`42`, `true`, `"ok"`) is different: the
engine's own item envelope only ever carries an object or array under
`json` — anything else normalizes to `null` on the way to a downstream
binding, a fixed property of the runtime this postcondition layer cannot
change. `field_present` on the bare `field = "json"` root therefore REFUSES
a scalar reply rather than certifying a value nothing downstream could ever
read: it fails with a gap sentence naming the scalar and suggesting the
fix — have the agent reply with an object naming the value (e.g.
`{"value": 42}`) and target the dotted path (`field = "json.value"`), which
already works via the object-merge case above. A dotted path *under* `json`
(`json.count`) is unaffected by this — reaching a scalar there means the
reply was already an object, which merges into the emitted value intact, so
`item.json.count` really does resolve downstream. A node with **no**
declared postcondition is entirely unaffected by any of this — its output
stays the plain `{text, agent_ref}` shape it always had, regardless of what
the agent happens to reply, so this feature changes nothing for a workflow
that never opted into it.

## The engine-only kinds OpenCompany rejects

The engine catalog carries four kinds the parser does **not** accept:
`code`, `memory`, `dedup`, and `loop`. A workflow file naming any of them fails
at parse with the usual unknown-kind error — they never reach `translate()`.
Each is left out for a specific, recorded reason (rationale lives in
[`src/workflows/caps/mod.rs`](../../../src/workflows/caps/mod.rs)):

| Engine-only kind | Why it is rejected |
| --- | --- |
| `code` | The `CodeRunner` capability is an explicit loud-failure stub (`UnwiredCode`): code execution for company workflows is not built, so the capability returns a clear error rather than a silent no-op. Accepting the kind would only let an author author a node that can never run. |
| `memory` | Deliberately undecided, not merely unbuilt. A `MemoryProvider` would give a workflow read **and write** access to agent memory, and which scopes a workflow may touch has not been settled. The capability is left `None` and pinned by `the_memory_capability_is_left_unwired_on_purpose`, so the answer must be given rather than defaulted into. |
| `dedup` | A tinyflows 0.6 catalog addition (arrived with the #499 pin bump) that OpenCompany has not adopted into the authoring set. |
| `loop` | A tinyflows 0.6 catalog addition (arrived with the #499 pin bump) that OpenCompany has not adopted into the authoring set. |

Rejection happens **at parse**, before translation — an author cannot smuggle
one of these into a running graph. Widening the parser to accept any of them is
a deliberate feature decision (settling the memory-scope policy, building the
code runner, or adopting the 0.6 additions), out of scope for this contract.

## The builder tier is narrower still

The host's automatic workflow builder — the plan → workflow bridge in
[`workflow-build.md`](workflow-build.md) — emits an even smaller set,
`BUILDER_NODE_KINDS` in
[`src/harness/workflow_build.rs`](../../../src/harness/workflow_build.rs):
`trigger`, `agent`, `condition`, `output`. A graph the builder proposes stays
inside those four kinds; a human author editing the file directly may use the
full 12-kind parser set. This is the strict nesting from the top of this
document: builder ⊂ parser ⊂ engine.

## Provenance

The engine-side counts and the specific 0.6-only kinds above are true **as of
the tinyflows 0.6.x pin (#499)**. When that pin is bumped, re-verify this
document against the new `NODE_KINDS` and re-decide whether any newly added
engine kind should be adopted into `WORKFLOW_NODE_KINDS`. Grep this file for
"#499" when bumping the pin.
