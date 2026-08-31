# Overview — the knowledge graph

`#/overview` is the graph and nothing else: no page header or strip. No view
has a top bar; its remaining controls live in the sidebar. The graph fills the
shell's content surface beside the sidebar.

A company with **no desks** still draws: the model hangs a deskless worker, and
an unplaced workflow, off the core node, so the roster, its tools and the saved
flows are all there without a single pillar. The corner says "No desks yet" and
offers the one control that changes it. The full-canvas empty state is reserved
for a graph with nothing but its core node — a fact about the graph, not about
desks. Those were one flag until recently, and it suppressed the canvas.

Issue #1321 moved it to `#/company/graph` and gave `#/overview` to an operator
landing page. That swap is undone — the graph is the overview again — and
`#/company/graph` is kept as an alias, so links minted while it lived there
still resolve. `OperatorOverview.tsx` remains in the tree but nothing routes
to it.

## What it draws

Five concentric rings, read outward from the centre:

| Ring | What | Where it comes from |
|---|---|---|
| 0 | the company, drawn as its memory constellation | `lib/memory.ts` (local store) |
| 1 | desks | `…/desks` — the company's real desks |
| 2 | the jobs on each desk, and the workflows it runs | `…/tasks` grouped through their assignee; `…/workflows` for the flows |
| 3 | the teammate who does each job, each workflow stage, and the humans | `…/team` matched by `task.assignee`; humans from `…/users`; stages are the saved graph's nodes |
| 4 | that teammate's tools | `…/team` — the grants the host resolved for that agent |

Rings 2 and 3 each carry two kinds. A workflow sits beside the SOP tasks
because both are work a desk runs; a workflow's stages sit beside the
workers because a stage is where the flow meets the person who performs it.

Hover any node to trace its whole desk chain. Click a desk to grow it into
a bottom-up tree; click a job for its steps, a teammate or tool for its card.
Click the core to bloom the memory constellation, with type-to-find over it.
Drag the background to pan. `←` / `→` turn the desk wheel; `Escape` steps
back out.

The graph is one tab stop. After Tab enters it, `←` / `↑` and `→` / `↓` move
between visible nodes; `Enter` or `Space` selects the focused node just as a
click would. The graph announces those instructions and each node's kind and
name to assistive technology.

Panning is an offset on top of whatever the camera is framing, not a separate
mode: the shot still tracks its subject, just off-centre by the amount you
dragged, and re-framing (selecting a node, opening the core) resets it.

## Which nodes are named

Labels are rationed, because the graph has more nodes than it has room for
names (issue #1104).

| When | Named |
|---|---|
| at rest | the company, every desk, and the roster — agents and people |
| hovering | the node under the pointer — the lit chain behind it stays unnamed |
| in a focused tree | the node you clicked and its direct children — a desk names its tasks, an agent names its tools |

Selection ranks just below hover, so it keeps its name against everything
except the node under the pointer. Every node keeps a `<title>`, so a native
tooltip is the floor wherever a drawn label is suppressed. Tasks, workflow stages and tools are the numerous tiers, which is
why they are bare until you point at one.

Whatever that leaves is then decluttered: candidates retain their full titles,
are placed highest priority first, and any label whose box overlaps one already
placed is dropped rather than nudged. The pass measures in **screen px**, not
graph units — labels hold one on-screen size at every camera depth
(`fixedLabel` counter-scales through `--kg-cam-k`), so zooming changes how far
apart nodes are and never how wide a name is. `kg/label-plan.ts` holds both
steps, pure.

## What is derived — read this before trusting the org chart

**One thing, and it is not a ring.** Everything the graph draws is now a value
the company declared. What this console still decides for itself is **where a
workflow hangs on the wheel**: the host scopes a flow to the *company*, and
nothing links a flow to a desk, so a flow is drawn on the desk of the first
teammate it runs through. That is a real relationship read as a placement it was
never declared to be. `DERIVED_NOTICE` in `kg/adapter.ts` is the standing
caveat; the legend visibly labels its placement as inferred, and its native
disclosure opens the full explanation for pointer, keyboard, and touch users.

Three rings used to be invented, and were deleted as the host grew the reads
they were standing in for:

- **Ring 1, desks** — `assignDepartment` keyword-matched a role string into
  one of five hardcoded buckets, falling back to Operations. The company's
  **desks** (issue #486) are now a `[[group_chat]]` in the manifest or an
  operator-created overlay desk, the same source the Company org chart reads.
- **Ring 4, tools** — `assignTools` dealt each teammate a slice of the
  company-wide `[tools] allow` list, positionally. A teammate's tools are the
  grants the host resolved for it (issue #601), carried on the roster read from
  the same server-side constructor the agent detail card uses, so the graph and
  the card cannot disagree about who holds what.
- **Ring 2, workflows and their stages** — `WORKFLOW_ROUTINES` dealt one made-up
  routine per desk by position, and `model.ts` dealt its stages round-robin
  across that desk's agents. A workflow is one of the company's saved graphs
  now, its stages are that graph's nodes in run order, and a stage hands off
  only to the agent the flow itself names.

A tool node is labelled with the grant **verbatim** — `mcp:*`, `workspace.*`,
`*`. It is the literal string in `company.toml`, which is what an operator greps
for; title-casing produced labels that appear nowhere in the company's config.

The console deliberately does **not** intersect a grant against the tool names
discovered from MCP servers. Deciding what a glob covers is the host's
`grant_matches`, and a second copy here is exactly the drift issue #264 forbids.

## The way out

Every card ends with one link to the page that names the same record
(`kg/open-in-console.ts`, issue #1308):

| card | goes to |
|---|---|
| AI teammate | `#/team/<agentId>` |
| SOP task | `#/tasks/<taskId>` |
| human | `#/settings/people` — there is no per-person address |
| note | `#/memory` |

A **tool** and a **stage** deliberately get none. A grant is a string in
`company.toml`, not a record with a page, and pointing at the MCP tab would
name a server the grant may have nothing to do with — the `grant_matches`
guesswork this file forbids two sections down. A stage is a node inside a saved
graph: the flow has an address, the node within it does not.

The destination is read off the **node's own id**, which already carries the
host's id for the thing it draws (`emp:<agentId>`, `team:desk:<deskId>`), so a
card cannot link to a different record than the node it was opened from.

It is a real `<a href="#/…">` rather than a click handler, so cmd-click,
middle-click and copy-link work and the hash router picks it up from
`hashchange` like every other link in the console.

## Freshness

The page is a **snapshot**, not a live view, and says so: a "Snapshot HH:MM" chip
sits top-right with a Refresh control that re-reads on demand.

There is no polling, on purpose. One paint is five reads — board, roster, desks,
people, memory — plus the workflow list and one graph read per saved workflow.
On a timer that is a standing cost for every open tab, for a picture that changes
when an operator does something rather than on the clock. The workflow reads are
bounded by how many flows the company has saved, not by roster size, so this is
not an N+1. What was never defensible was staying silent about the staleness.

### Who the graph does not place

Ring 1 can only place somebody the company seats. Three things it cannot, and
the graph says so rather than guessing:

- **A teammate on no desk.** They are on the roster and nowhere in the
  structure, so they hang off the company core in a sector of their own, with no
  desk above them. Their open board cards are dropped, because ring 2 hangs
  off ring 1 and there is no honest desk to hang them from.
- **A human.** Desks staff agents, so the company declares no desk for a person
  and this graph does not guess one — the same answer the org chart gives, for
  the same reason. `assignHumanDepartment` is gone with `assignDepartment`:
  spreading humans across *invented* buckets was self-consistent fiction, but
  spreading them across *real desks* would assert a membership the desk's own
  member list contradicts.
- **A workflow that runs through nobody seated** — a pure trigger/HTTP routine,
  one whose agents all sit on no desk, or one the host lists with no saved graph
  behind it. It hangs off the core too. Unlike the board cards above it is *not*
  dropped: the company really does declare the flow, so it is drawn stageless
  rather than going missing with no error anywhere.

An empty declared desk draws no desk: `buildKnowledgeGraph` only draws a desk
somebody claims. That is pre-existing behaviour, not a decision this
made.

Everything else is real: a card's assignee, the desks that seat each teammate,
the grants the host resolved for each agent, the nodes of each saved workflow,
and who can sign in.

## Files

`kg/` holds the graph itself — `model.ts` (the five-ring node/edge model),
`adapter.ts` (our host's data, shaped into it), `tree-layout.ts` and
`memory-core.ts` (pure layout and camera maths), `label-plan.ts` (which
labels survive), and the `KnowledgeGraph` /
`KnowledgeGraphFullscreen` / `KnowledgeDetail` components. `pulse.ts` holds
the two board predicates the adapter needs. Theme tokens live under `.oc-kg`
in `src/index.css`.

The graph is the whole page, so its chrome stays minimal: a desk selector, a
kind legend, the side paddles, and the detail card. The selector names every
desk rather than numbering them (issue #1309) — it used to draw one anonymous
dot per desk under the words "Pick a desk", so the control that exists to
choose a desk said which was which only on hover, and said nothing at all to a
touch device. The chips wrap rather than scroll or clip; each carries the
colour its desk's node and label already carry. The docked directory index
and the entity/function/action lenses were removed — with nothing else on the
page competing for attention, they covered more of the graph than they earned.

The legend is bounded by the graph canvas and wraps its kind chips rather than
running beyond the clipped edge. Below 900px the paddles become 80px by 40px
and inset 12px; below 640px they become 56px by 32px and inset 8px. Keyboard
`←` / `→` remains available at every width, so the smaller touch targets do not
remove a way to turn the pillar wheel.

Opening a card moves the legend as well (issue #1664). The detail panel is a
300px right rail above 820px and a bottom sheet at or below it, and the legend
used to sit at `z-10` under its `z-30` on the same bottom edge — so on a phone
every kind label and the workflow-placement caveat disappeared the moment a node
was selected. It is now `z-40`, the level the snapshot line and the paddles
already use; above 820px its width stops short of the rail, and at or below it
the legend lifts to sit on top of the sheet, capped so it scrolls rather than
climbing into the desk selector. The sheet's height and the paddles' offset are
percentages of the graph card rather than `vh`: the card is shorter than the
window, so `62vh` was 68% of the box the sheet actually lives in, and the band
above it that every other offset was derived from never existed. Where the
legend ends up is measured in a browser at both sides of the breakpoint by
`test/e2e/overview-responsive-chrome.spec.ts`, with the caveat shut and open —
open is the legend's real height.
