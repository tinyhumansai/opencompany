# Planning

*The board's Planning station (issue #337, epic #183 §4).*

A card dragged into **Planning** is turned into a plan and then settled — it
does not rest there. This document is the contract for that pass: what triggers
it, what it is allowed to do, how it decides whether the work can start, and
what happens when any of it goes wrong.

Implementation: `src/harness/planning.rs` (the pass), `src/ports/tasks.rs`
(the `TaskPlan` shape), `src/metering/planning.rs` (what it costs and who pays),
`src/runtime/advance.rs` (the boot sweep).

---

## The contract in one table

| | |
|---|---|
| **Trigger** | the transition *into* `planning`, edge-fired in `CompanyRuntime::upsert_task` |
| **Work done** | exactly one model call, no tools, no retry |
| **Deadline** | 120s, hard |
| **Cost** | one `SampleKind::PlanningCall` sample, charged to the company |
| **Run row** | none |
| **Locks** | none |
| **Exit** | automatic, three-way (below) — the card never stays in `planning` |

### The three exits

| Outcome | Landing | Card carries |
|---|---|---|
| planned, nothing blocking, a valid assignee | `in_progress` | the plan; the dispatch edge fires |
| planned, a hard prerequisite missing (or no valid assignee) | `todo` | the plan **and** the named gap |
| the pass failed (model error, timeout, unparseable output) | `todo` | the reason only — **no** plan |

A failed pass writes no partial brief on purpose. A plan half-produced by a
model that errored mid-answer reads exactly like a finished one, and an
operator would act on it.

---

## Why entering Planning dispatches on success

The success exit hands the card straight to `in_progress` with no human
acceptance step in between. That is the design, not an oversight.

Epic #183's diagram presents plan → dispatch as the automatic spine of the
board; requiring an acceptance click would re-insert the hand-carry the issue
exists to remove, and would leave every planned card waiting on a person who
has already said what they want. The operator's levers are all still there:

- the drag into Planning is **opt-in, per card** — nothing routes work through
  planning automatically;
- `todo → in_progress` still dispatches unplanned, so planning is never
  compulsory;
- a missing prerequisite stops the walk **before** any dispatch spend;
- the run still stops in In Review, so nothing reaches Done without a person.

The cost this accepts: a drag into Planning can spend the assignee's budget
without a second confirmation. The drag is informed consent (the console says
so), the per-agent cap from #304 still gates the dispatch turn, and the run
still stops for review.

**Planning is a spend gate.** Before #337, `in_progress` was the only column
whose entry cost money. There are now two. `frontend/src/api/tasks.ts` says so
where it used to claim there was one.

---

## Evidence before prescription

The obvious design — give a planning agent tools and let it go look — is a tool
loop, which is an agent, which is a dispatch. That is the thing planning is
supposed to happen *before*.

So the direction is inverted: the **host** gathers the evidence
deterministically and the model only synthesises.

The pack, all of it read at the start of the pass, from one snapshot:

| Evidence | Source |
|---|---|
| the card | title, note, priority, current assignee |
| roster + desks | `manifest.agents`, `manifest.group_chats` |
| per-teammate tool grants | `runtime::builder::agent_effective_grants` |
| connections | `server::ops::connections_read::project_connections` — the same projection `GET …/connections` builds |
| MCP servers | `company::mcp::resolve_effective` — manifest `[[mcp_server]]` ∪ the runtime index |
| workspace | `WorkspaceStore::tree`, bounded to 200 nodes |
| skills | enabled `SkillStateStore` entries |
| approval policy | `manifest.policy.mode` + `always_approve` |
| credential presence | booleans only — `runtime.mail().is_some()`, Composio token set |

**Only names and booleans ever enter the prompt.** No credential value is read,
let alone rendered; presence uses the same `get(…)`-reduced-to-a-bool probes
`composio::token_configured` and `mcp::auth_configured` use. A test asserts a
stored OAuth token never appears in the prompt.

The model runs with `ModelRequest::tools` empty. There is no loop, no second
call, and no path by which a planning pass can act on the world.

---

## The model claims, the host verifies

The model emits prerequisites as `{kind, name, why}` and **cannot** emit a
status — `PrereqClaim` has no such field, so the asymmetry is enforced by the
type rather than by the prompt asking nicely.

The host stamps every verdict:

| Kind | Checked against | `missing` reads as |
|---|---|---|
| `connection` | the `GET …/connections` projection | "GitHub is not connected — connect it from the Connections tab" |
| `composio` | the same projection's `via`, plus token presence | "no Composio account is connected for this" |
| `mcp` | manifest ∪ runtime index — **both** halves; a disabled server is its own verdict | the named server is in neither |
| `credential` | presence only — the mail handle, or a secret key that exists | "no outbound email is configured" |
| `file` | the workspace tree, matched on the trailing path segment | "references a path that is not in the workspace" |
| `permission` | the **working** teammate's manifest grants + `[policy]` | not granted, or granted under a read-only policy |
| `assignee` | `runtime::assignee::resolve` over the whole roster | nobody by that name is on the roster |
| anything else | nothing — the host says so | — (always `unknown`) |

### The verdict taxonomy

| Verdict | Blocks? | Means |
|---|---|---|
| `satisfied` | no | the host looked and found it |
| `missing` | **yes** | the host looked and it is not there |
| `needsApproval` | no | present, but policy stops it for a person when used |
| `unknown` | no | the host could not check |

**An inventory that could not be read yields `unknown`, never `missing`.** That
is the deliberate failure direction: a Composio outage must not make every card
in the company unplannable. The cost is stated under *Risks* below.

Two deliberate looseness choices, both erring toward *not blocking*, because a
false refusal is the expensive way to be wrong here:

- **`file` matches on the trailing segment**, case-insensitively. A model writes
  `Standards/Tone.md` or `Tone.md` for the same note; a path-shape mismatch that
  blocked a card would be a false refusal. Two same-named files in different
  folders can therefore satisfy the check.
- **`permission` reads the manifest only** — the tool allow-list, the agent's
  own list, and `[policy]`. Not `runtime::grants` and not the harness
  `ApprovalPolicy`: those are per-call runtime machinery that legitimately
  changes between planning and dispatch, and reading them here would couple a
  plan to state that is allowed to move. A desk resolves to its **lead**, who is
  who actually runs the turn.

### The assignee rule

A plan may **fill in** a blank assignee but never **reassign** one a person
chose. The operator's routing decision is not the planner's to overrule; a
proposal is still recorded on the brief either way, so the suggestion is visible
without being applied. A proposal the roster does not recognise is dropped
rather than shown.

---

## No run, and therefore no lock

A pass mints no `RunRecord` and takes no lock. Both are load-bearing.

**No run row.** A run is an *attempt at the work*: it has an agent, a trace, a
cost attributed to a teammate, an operator who can steer or cancel it. A
planning pass has none of those. A row here would put a phantom attempt in the
runs list and on the card's timeline, and would make "how many times has this
card been tried?" wrong.

**No cycle lock.** The runtime's per-company `serial` lock is held for a whole
agent turn. Taking it would park every planning pass behind whatever the company
happens to be doing — and, worse, park the company behind a planning pass.

Three cheaper guarantees replace them:

1. **Edge-firing at the single write site.** The trigger is the transition, and
   `CompanyRuntime::upsert_task` is the one path every REST task mutation takes.
   A card re-saved while already in Planning is not a transition, so an edit, a
   re-title or the pass's own note append cannot start a second pass. This gives
   *one pass per entry, no retry* by construction.
2. **A per-company in-flight set.** Covers the case the edge cannot: dragging a
   card out of Planning and back in *is* a second genuine transition. Check-and-
   insert before the spawn, released by a drop guard — so a panic, a timeout or
   an early return cannot leak a claim and make a card permanently unplannable.
3. **An optimistic settle guard.** The pass captures the card's
   `updated_at_millis` before the model call and, at settle, requires both that
   the column is still `planning` **and** that the stamp is unchanged. Any
   operator action bumps the stamp, so the operator's move wins and the pass
   discards its entire result.

A discarded pass **stays metered**. The tokens were genuinely spent; a meter
that only counted the passes that happened to land would under-report real
money.

### Crash recovery

There is no run row for the boot reaper to find, so a host that dies mid-pass
would otherwise leave a card in Planning that nothing will ever re-drive — the
trigger already fired. `runtime::advance::sweep_stranded_planning` reads the
board directly at boot and returns every Planning card to To-do with the
restart reason on its note.

It is sound because Planning is **transient by construction**: every
terminating path leaves the column, so a card found there at boot provably
belongs to a dead process. Like the run reaper, it is suppressed on a *rebuild*,
where that premise is false and it would yank a live pass out from under itself.

---

## Cost attribution

One `SampleKind::PlanningCall` sample per pass, `agent: "company"`
(`metering::UNATTRIBUTED_AGENT`), `run_id: None`. Money posts through the
existing `inference.spend` ledger entry when cost > 0. Both writes are
logged-and-swallowed — metering never fails the work it meters.

Charging the assignee would be wrong twice:

- planning is frequently what *picks* the assignee, so a card with a blank
  assignee has nobody to charge, and billing the teammate the pass chose would
  charge a decision to its own outcome;
- since #304 per-agent daily caps are enforced, so a teammate near its limit
  would make its own cards unplannable — the operator would get a refusal about
  a budget they were not trying to spend.

`"company"` is not a roster agent, so it is uncapped by #304. It **does** count
toward the capability-tier token ceiling (#108) via `metering::tokens_in`;
excluding it would let a company plan indefinitely after crossing the budget
that was supposed to stop it.

`bucket_usage` needed no change — it sums kind-agnostically.

---

## The brief is a structured field

`TaskRecord.plan: Option<TaskPlan>`, serde-default + skip-if-none — the additive
wire precedent of `origin_chat_id` / `parent_task_id`, so no stored board needs
migrating on fs, sqlite or mongodb.

Not an **artifact**: #244's rule is that artifacts are deliverables published
*from a run*, identified by `(task, source)`. There is no run, so an artifact
here would be a deliverable with no producer and would appear in the Artifacts
tab as though the work had output.

Not **note prose**: the note is the card's append-only history and reads as a
transcript. Verdicts and estimates need structure the console can badge and the
next pass can overwrite. The note still gets one appended outcome line per pass,
so the history channel stays complete.

Re-planning **overwrites** `plan`; the note keeps the trail of both passes. A
vector of briefs would make "the plan" ambiguous at exactly the moment an
operator wants an answer.

Everything the model produced is capped before it is persisted — 12 steps, 12
prerequisites, 8 risks, and per-field codepoint limits — so a model that decides
to write an essay costs a truncated brief rather than a board that will not
load.

**Estimates are guesses and nothing derives a budget from them.** The real caps
are `budget_usd_daily` and the capability tier, both enforced from live meter
reads. The console renders them hedged, for the same reason.

### Wire surfaces

`plan` is projected on the REST card DTO (`TaskCard`), verbatim rather than
reshaped — a second transcription is how the badge the console renders drifts
from the verdict the dispatch gate used.

**GraphQL deliberately omits `plan` in v1.** The console reads the board over
REST; adding a nested object graph to the GraphQL `Task` type buys nothing today
and would have to be kept in step with the port shape forever. The SDL snapshot
pins the omission, so adding it later is a deliberate edit rather than a drift.

Intake cannot set `plan` at all: `POST …/tasks` has no field for it, so a client
cannot post a card that already claims to be planned, and cannot forge the
prerequisite verdicts that decide whether it dispatches.

---

## Risks accepted

- **Auto-dispatch on success** spends the assignee's budget from a Planning
  drag. Accepted: the drag is informed consent, the per-agent cap still gates
  the dispatch turn, and the run stops in In Review.
- **A Composio or MCP outage yields `unknown`**, so a genuinely-missing
  connection can slip through and the run then fails with a real error. That is
  today's behaviour for every card — just rarer.
- **Prompt injection via card text** is bounded rather than eliminated: the
  parse is typed, verification is deterministic and host-side, there are no
  tools, and no secret is in the prompt. The worst a hostile card can do is
  produce a misleading brief a person reads.
- **A discarded pass costs one metered call** with no landing.
- **Estimates are model guesses**, labelled as such. Nothing derives budgets
  from them.
