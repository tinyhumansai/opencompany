# Agentic Research Lab

> A research lab of agents that investigates a question with primary sources,
> computes what it can, argues with its own conclusions, and reports only what
> it can defend — with a human setting the question and accepting the findings.

Where the other shipped companies model a business, this one models an
**investigation**. It is also the acceptance test for
[`docs/spec/runtime/orchestration/`](../../docs/spec/runtime/orchestration/README.md):
if a lab converges on a question without an operator babysitting it, that work
landed.

## What it can do

- Establish what it already knows before spending anything on finding out again.
- Discover and download primary sources, following what its own sources cite.
- Record what each source *actually establishes* — one statement at a time, with
  the conditions it depends on and how well it is established.
- Compute rather than estimate, keeping the programs it wrote.
- Attack its own conclusions before an operator ever reads them.
- Report what survives, with the evidence attached.

## Agent roster

| Agent | Responsibility |
| --- | --- |
| Research Lead | Break the question into lines of inquiry, delegate, combine. |
| Librarian | Find and download primary sources. Never reads them. |
| Scholar | Read what was gathered; record what it establishes. Never fetches. |
| Analyst | Compute, model, and check the numbers a claim rests on. |
| Tool Builder | Write and run the programs, and keep the shared library. |
| Inventor | Propose a different angle when the current line stalls. |
| Critic | Attack the lab's own conclusions before the operator sees them. |
| Curator | Owns the brief: one current statement of what the lab knows. |

## Three design choices worth knowing

### The librarian and the scholar are different agents

One fetches and never reads; the other reads and never fetches. This looks like
bureaucracy and is not: splitting them lets each instruction be strict about one
thing, and it removes the failure where a role that already knows what the lab
wants to be true goes looking for exactly that.

It also makes a hard rule enforceable — never download a URL that did not come
from a search result, a citation, or a source already held. A fetch of an
invented address *succeeds*, and files the wrong document under the name the
model wanted. The sibling runtime this is ported from recorded exactly that: a
paper on graded Lie algebras stored under the name of an unrelated theorem.

### The critic is told less than everyone else

It sees the goal, the claims, and the index — and deliberately **not** the
assertion board or the brief.

Not the board, because a post there is asserted rather than established, and a
critic weighing evidence beside an unevidenced hunch is one prompt away from
weighing the hunch. Not the brief, because the brief is the lab's own summary of
what it believes, and a critic handed it argues with the summary instead of with
the evidence.

Context is authority. What a role is *not* given matters as much as what it is.

### There are no desks

Every other company with collaborating agents declares `[[group_chat]]` desks.
This one declares none, and routes collaboration through
[`workflows/research_loop.toml`](workflows/research_loop.toml) instead.

That is the point. A desk's entire behaviour is "resolve a lead, run that
member's turn, relay the reply" — which a workflow `agent` node already does,
with retries, error routing, approval gating, nesting and cancellation on top.
See
[delegation.md](../../docs/spec/runtime/orchestration/delegation.md#desks-are-workflows).

## Human in the loop

Humans **set the question and accept the findings**; the lab runs everything
else. Policy-generated HITL is disabled. When the lab genuinely needs a
decision before an action, it asks explicitly with `request_approval`.

Acceptance stays human on purpose. A result the lab can defend is not the same
as a result the operator wants, and the runtime keeps those two questions
separate: see
[answered is not accepted](../../docs/spec/runtime/orchestration/demand-ledger.md#answered-is-not-accepted).

## Running it

```sh
cargo run --bin opencompany -- serve --company companies/agentic_research_lab
```

Search requires a key; without one the lab still runs, and the librarian simply
has nothing to discover with. Set `[tools].search_daily_calls = 0` in
`company.toml` to pause search spend without editing the grant list.

## Status

The roster, the grants, the workflow and the context routing are real and parse
today. The mechanisms they are built to exercise are specified but not all
implemented — the `context` key in particular is carried as data and not yet
consumed, and the workflow expresses one pass rather than a retrying loop. See
the [orchestration spec](../../docs/spec/runtime/orchestration/README.md) for
what lands in which phase.

## Tool servers

A source-backed report needs primary sources it can cite: published models, datasets and papers, and the actual implementation behind a claim.

Declared in [`mcp.json`](mcp.json) and merged with anything the install
ships and anything an operator adds from the console. A server marked
*needs a token* is declared but off: write its credential from
Settings → Connections, then enable it there.

| Server | What it is for | Ships |
| --- | --- | --- |
| `deepwiki` | Documentation and Q&A for any public GitHub repository. Public and no-auth. | on |
| `context7` | Version-accurate API and library documentation, so answers match the release in use. | on |
| `huggingface` | Models, datasets and papers on the Hugging Face Hub. Public and no-auth. | on |
