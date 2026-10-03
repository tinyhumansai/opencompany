# Orchestration: making a many-agent company converge

OpenCompany can already *run* many agents. This section specifies how it makes
them **converge** — stay pointed at one goal, share one document set, and stop
re-establishing what the company already knows.

The design is a port of mechanisms proven in a sibling runtime: a Dockerised
mathematical research agent that solves Project Euler problems and attacks open
conjectures by running 22 specialist roles concurrently against one shared
workspace, on a cheap model, for under $100 a run. Its roles converge; ours do
not yet. Terms: [glossary](../../glossary.md).

Both runtimes vendor `tinyagents` and run on `tinyflows`, so this is a port onto
shared substrate rather than a new runtime.

## The three collapses

This section is not only additive. It removes three things.

1. **The [demand ledger](demand-ledger.md) replaces the kanban board** as the
   work model, scoped to agents. Work is *stated by whoever is blocked*, deduped
   against what the company already knows, and closed by evidence that cites it.
2. **[Desks are rooms, not relays](delegation.md#desks-are-rooms)**. A desk
   used to be a `{id, name, description, members}` record whose entire
   behaviour was "resolve the lead, run that member's turn, relay the reply",
   and the plan was to fold it into the workflow engine. It is now an
   `OpenHumanHive` whose seats run concurrent rounds and speak to each other
   ([../hive.md](../hive.md)); the synchronous hand-off tools
   (`delegate_to_desk`, `delegate_to_teammate`) and the in-turn relay went
   away instead. Workflows stay what they are: the static, inspectable graph
   for work that is a pipeline rather than a conversation.
3. **[Memory becomes generic](memory.md)**. Three bespoke ports plus a
   hand-rolled `CortexClient` collapse onto one `MemoryEngine` contract.

## The three principles

Every mechanism here is an application of one of these. They are the reason to
port the design rather than the features.

### A prompt instruction is not a control

Anything that matters is enforced by registration, routing, or a lock — never by
asking a model to abstain. OpenCompany already argues this for tool reach in
[`src/harness/built_in/confine.rs`](../../../../crates/opencompany-core/src/harness/built_in/confine.rs)
("an empty belt already means the model is offered nothing; the policy is what
makes that a boundary rather than an absence"), and for speech in a room —
"exactly one action per turn" is an MCP error on the second call, not a
sentence in the prompt ([../hive.md](../hive.md#speaking)). This section
applies the same standard to
*alignment*: which files a role sees, what may close a unit of work, and who may
assert what.

### Derived, never asserted

A summary an agent writes drifts from what it summarises. A summary **code**
re-derives, whole, from a directory of one-file-per-item cannot. Every ledger
here is re-rendered in full on every relevant write and never edited in place.

That is also what makes concurrency cheap: two agents writing distinct items
conflict on nothing, so a single mutex around the re-render is the entire
coordination story **for derived-ledger rendering**. Writing the underlying
per-item files and committing them are separate concerns with their own locks
— see [alignment.md's write lock and commit lock](alignment.md#locking).

### Nothing blocks on a slow participant

Work reaches the next boundary through a mailbox or an append-only queue, never
by making one participant wait on another. The sibling runtime recorded a run
that spent 56 of its 74 minutes unable to start its second attempt because a
support agent had been made a gate.

## What OpenCompany already does better

The port is selective. These stay as they are:

| Concern | Why ours wins |
| --- | --- |
| Role definitions | `[[agent]]` in `company.toml` is data; the sibling's 22 roles are hardcoded Rust, and adding one means editing three files |
| Tool grants | `Agent.tools` globs + `GATEABLE_NAMESPACES` + capability filtering, versus per-definition arrays |
| Storage | ports with a conformance suite across fs / sqlite / mongodb |
| Approvals, metering, budgets | no equivalent exists in the sibling runtime |
| Workflow engine | strictly more capable than the sibling's role benches |

So the port takes the *discipline*, not the shape. Where the sibling encodes a
routing table as a Rust `match`, ours reads it from the manifest.

## The gap, stated plainly

| Mechanism | Ours today | Spec |
| --- | --- | --- |
| Which files reach which role's prompt | nothing — every agent gets the same charter | [alignment.md](alignment.md) |
| A budgeted, single-writer shared brief | absent | [alignment.md](alignment.md) |
| Code-derived ledgers | absent; the workspace is agent-written prose | [alignment.md](alignment.md) |
| An assertion board distinct from the work board | absent | [alignment.md](alignment.md) |
| Work stated by the blocked party, deduped, closed by evidence | absent; cards are pushed by a planner and closed by a drag | [demand-ledger.md](demand-ledger.md) |
| Retry after a failed attempt | absent; [planning](../planning.md) is one tool-less call with no retry | [loop.md](loop.md) |
| A judge separate from a verifier | absent | [loop.md](loop.md) |
| Waiting for delegated work inside the turn that asked for it | **absent — no join primitive exists** for board work; a room waits for its seats by construction (a round commits when every seat has spoken) | [delegation.md](delegation.md) |
| Directing a run in flight from outside it | half — enforcement exists, the operator queue does not | [delegation.md](delegation.md) |
| Containerised code execution with a persistent library | the sandbox is vendored but not enabled | [sandbox.md](sandbox.md) |
| One memory contract | three ports plus a bespoke client | [memory.md](memory.md) |

## Phasing

Ordered by dependency, not by value. Each phase is independently shippable.

| Phase | Doc | Depends on |
| --- | --- | --- |
| P0 — one memory contract | [memory.md](memory.md) | nothing |
| P1 — the alignment layer | [alignment.md](alignment.md) | nothing |
| P2 — demand replaces the board | [demand-ledger.md](demand-ledger.md) | P1 (claims close demands) |
| P3 — the attempt loop | [loop.md](loop.md) | P2 (an attempt is against a demand) |
| P4 — await, directives | [delegation.md](delegation.md) | P3 |
| P5 — containerised tools | [sandbox.md](sandbox.md) | nothing |
| P6 — the research template | [../../../../companies/research_lab/README.md](../../../../companies/research_lab/README.md) | all of the above |

The desk collapse that used to sit in P4 is withdrawn: desks became rooms
([../hive.md](../hive.md)), and a room needs no join primitive — a round is
the join. `await_task` is still owed to **board** work, where a card's turn
has nowhere to wait for the card it spawned.

## Inspectability

Both the alignment layer and the demand ledger are hard to review by running a
company — the interesting states take hours to reach. The sibling runtime solves
this with two host-side commands that need neither a container nor an API key:
one renders every derived ledger from a workspace on disk, the other renders
every role's fully assembled system prompt with token counts.

OpenCompany SHOULD ship the equivalent as `opencompany inspect`. A routing table
nobody can print is a routing table nobody reviews.

## Non-goals

- **Competing method overlays.** The sibling runtime can run two or three
  "schools" — different attack methods sharing one workspace and one board.
  It is a real idea and it is out of scope here; note also that its code is
  behind its documentation (the per-role overlay table is empty and the
  documented per-school bench does not exist as a field).
- **Domain-specific ledgers.** Proof skeletons, weakening ladders, entailment
  closure, and citation frontiers are mathematics. The two ledgers specified in
  [alignment.md](alignment.md) are the domain-general ones.
- **Replacing the operator.** Every mechanism here changes how agents converge,
  not who decides. *Agents propose; the Operator disposes*
  ([agentic/](../../agentic/README.md)) is unchanged — see the
  [answered-versus-accepted split](demand-ledger.md#answered-is-not-accepted).
