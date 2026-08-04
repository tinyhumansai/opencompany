# Overview — the agent graph

The console's landing surface at `#/overview`. The whole page is one diagram:
the company at the centre, its hubs around it, and everything those hold on the
ring beyond. The chrome floats over the canvas rather than boxing it in.

```
                 ┌ state line (top-left)
                 │
      ╭──────────┴───────────────────────────╮  ┌ inspector (right)
      │              ·  ·  ·                 │  │ describes whatever
      │          ·   ╭─────╮   ·             │  │ the camera is on
      │        ·     │ CO. │     ·           │  │
      │          ·   ╰─────╯   ·             │  └ directory at rest
      │              ·  ·  ·                 │
      ╰──┬───────────────────────────────────╯
         └ legend · lens (bottom-left)          ┌ live strip (bottom)
```

## The graph

Three branches, one hue each, every edge one the host actually records:

| Hub (ring 1) | Leaves (ring 2) | The edge |
|---|---|---|
| Teammate | their cards | `task.assignee` |
| Skill area | its skills | `skill.category` |
| MCP server | its tools | what the server advertises |

Nothing is joined **across** branches — no line from a teammate to a tool —
because the host stores no such edge and drawing one would be a claim.

Wedges are density-weighted: a hub's angular span is proportional to how many
leaves it holds, so a teammate carrying six cards gets room to spread. With
every hub equally loaded it degenerates to even spacing, which is what an empty
company should look like.

## Reading it

- **Hover** lights the whole chain through a node — its parent, its children —
  and dims everything else, so one glance answers "whose is this". Leaf labels
  appear only for the lit chain; a hundred of them at rest would overlap.
- **Click** dives: the camera moves onto the node and magnifies, rather than
  redrawing the scene, so you never lose where it sits in the whole. Labels are
  counter-scaled against the zoom, so type stays the same size on screen and
  only the diagram grows.
- **Escape**, the inspector's back-link, or clicking the empty field dives out.
  Escape goes up exactly one level.
- **The legend is the lens.** Each row names a kind, counts it, and toggles it
  off. Hiding a hub takes its leaves with it — a tool with no server on screen
  has nothing to hang from.

Hubs read as hollow rings wearing their icon, leaves as solid dots. At 7.5px
across an icon is a blob, so shape separates the two tiers and hue says which
branch a leaf belongs to. Identity never rests on colour alone: every node has
a label on hover, an `aria-label`, and a named row in the legend.

## Where the numbers come from

Company status, approvals, `…/tasks`, `…/team`, `…/skills`, `…/mcp/servers`.
A host that does not serve one of those returns 404, the branch is simply
absent, and the legend does not list it — an empty row would read as "you have
no tools" when the truth is "this host has no tool API". A host without a
roster route falls back to `starterTeam()`, exactly as the Team page does.

Only connected MCP servers are asked for their tools; asking a disconnected one
spends a request to be told nothing.

## Files

| File | Holds |
|---|---|
| `graph.ts` | the model, the sunburst layout, the chain, the lens — all pure |
| `types.ts` | the chrome's view models |
| `palette.ts` | one hue per branch, from the console's shared palette |
| `AgentGraph.tsx` | the canvas: rings, edges, nodes, hover, the dive camera |
| `Legend.tsx` | the legend, which is also the lens |
| `Inspector.tsx` | the panel that re-scopes with the camera |
| `pulse.ts` | the state line and the live strip |
| `Ticker.tsx` | the live strip |

Behaviour is covered by `test/e2e/overview.spec.ts`, which drives the dive and
the lens end to end against a running host.
