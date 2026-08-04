# Overview — the command centre

The console's landing surface at `#/overview`. One screen that answers "what is
my company doing right now", in the order an operator reads it:

1. **State line** — one honest sentence assembled from live signals
   (`pulse.ts::stateOfWorld`). A quiet company gets a short line, not a padded
   one.
2. **Pulse row** — four tiles, each answering one question and diving into the
   surface that answers it in full. The footer visual under each value is
   hand-drawn SVG (`Tile.tsx`), so the top of the page never waits on a chart
   library.
3. **Live strip** — the most recent real movements, approvals first
   (`Ticker.tsx`). Pauses on hover; still under `prefers-reduced-motion`.
4. **Company map + focus panel** — the dive (see below).
5. **Two charts** — board activity over 14 days, and where cards are piled up.
   Lazily imported, because recharts is ~400 kB and both are below the fold.

## The dive

`CompanyMap.tsx` draws the company at the centre with the roster in orbit.
Clicking a teammate moves the camera onto them and fans their cards out around
them; clicking a card goes one level deeper. Diving is a zoom — the same scene
stays on screen and the camera moves — so you never lose where the thing you
are looking at sits in the whole. Labels are counter-scaled against the zoom so
type stays the same size on screen at every depth.

`FocusPanel.tsx` always describes whatever the map is centred on, so the two
read as one gesture. `MapFocus` (in `types.ts`) is the single piece of state
that drives both.

Diving out: Escape, the breadcrumb, the "Dive out" button, or clicking the
empty field. Escape goes up exactly one level.

## Where the numbers come from

Everything is derived from surfaces the host already serves — company status,
approvals, `…/tasks`, `…/team`, `…/skills`. Nothing is fabricated:

- The activity chart counts cards **touched**, not finished. The board records
  only `updatedAt`, and calling it throughput would overstate what we know.
- A host without a roster route falls back to `starterTeam()`, exactly as the
  Team page does.
- Empty is a real state with its own copy, not a zeroed chart.

## Files

| File | Holds |
|---|---|
| `types.ts` | the view models, including `MapFocus` |
| `pulse.ts` | every derivation, pure and clock-injected |
| `palette.ts` | the colour vocabulary, shared with Usage and Finances |
| `Tile.tsx` | the pulse tiles and their SVG spark / meter / segments |
| `Ticker.tsx` | the live strip |
| `CompanyMap.tsx` | the map and the dive |
| `FocusPanel.tsx` | the panel that re-scopes with the map |
| `Charts.tsx` | the two recharts figures (lazily loaded) |

Behaviour is covered by `test/e2e/overview.spec.ts`, which drives the dive end
to end against a running host.
